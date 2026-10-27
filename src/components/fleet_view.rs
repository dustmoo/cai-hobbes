//! The Fleet view (Pro) — every observed Claude Code session on the machine,
//! rendered in the planner's centre column when the Fleet rail entry is
//! selected.
//!
//! Reads the UI mirror `Signal<fleet::FleetState>` (fed by the drain loop in
//! `main.rs`). Actions go the other way through `fleet::shared()`: a
//! headless run Hobbes dispatched has its permission requests held, and its
//! row's Approve / Deny resolve the held gate's oneshot; an interactive
//! session's request is answered in its terminal (the row takes the person
//! there). Status colors are computed values, so they ride inline `style:`
//! attributes (Tailwind purges computed class names).

use chrono::{DateTime, Local, Utc};
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fi_icons, Icon};

use crate::fleet::{self, AttentionKind, FleetSession, FleetStatus};
#[allow(unused_imports)]
use crate::fleet::FleetOrigin;
use crate::todo::model::format_minutes;

/// Whether a day-rollup LLM call is currently in flight. `on_rollup` runs the
/// call in a root-owned `spawn_forever` task (not a component-scoped
/// `spawn`) so navigating away from Fleet mid-generation doesn't cancel the
/// up-to-90s request — but that means the task must not write into
/// component-local `use_signal`s, which get dropped with the component.
/// This (and its two siblings below) are the survives-unmount home for that
/// state; `FleetView` reads them directly so re-entering Fleet shows the
/// running/finished state instead of losing it.
static FLEET_ROLLUP_RUNNING: GlobalSignal<bool> = Signal::global(|| false);
/// Error from the most recent rollup attempt, if any.
static FLEET_ROLLUP_ERROR: GlobalSignal<Option<String>> = Signal::global(|| None);
/// A freshly generated rollup, staged here until some mount of `FleetView`
/// picks it up (see the `use_effect` in `FleetView` that drains this). The
/// meta-store write happens unconditionally inside the task regardless of
/// whether anything is mounted to observe this signal.
static FLEET_ROLLUP_FRESH: GlobalSignal<Option<fleet::briefs::DayRollup>> = Signal::global(|| None);

/// Take the staged rollup, subscribing the caller to `FLEET_ROLLUP_FRESH`.
///
/// Must be called from inside a `use_effect`: only `.read()` registers the
/// effect's reactive context (`.write()` notifies subscribers but subscribes
/// nothing), so without the read a rollup finishing while Fleet is mounted
/// would sit unseen until a remount. The read guard ends with its statement
/// — holding one across the clearing `write()` panics ("already borrowed") —
/// and the early return keeps an idle run from writing at all, since an
/// unconditional write after the read would re-dirty the effect forever.
fn take_fresh_rollup() -> Option<fleet::briefs::DayRollup> {
    if FLEET_ROLLUP_FRESH.read().is_none() {
        return None;
    }
    FLEET_ROLLUP_FRESH.write().take()
}

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
        FleetStatus::Waiting => ("waiting on a timer", "#60a5fa"),
        FleetStatus::Idle => ("idle", "#94a3b8"),
        FleetStatus::NeedsAttention(AttentionKind::Gate) => ("waiting on you", "#f59e0b"),
        FleetStatus::NeedsAttention(AttentionKind::Notification { .. }) => {
            ("needs you", "#f59e0b")
        }
    }
}

/// Visible label for a background phase — the one state where a silent dot
/// misleads (silence is its normal operating mode). Past the foreground
/// staleness window the quiet age is spelled out, so "silently crunching"
/// and "actively heartbeating" read differently at a glance.
fn background_label(status: &FleetStatus, last_event_at: DateTime<Utc>, now: DateTime<Utc>) -> Option<String> {
    if *status != FleetStatus::WorkingBackground {
        return None;
    }
    let quiet_min = (now - last_event_at).num_minutes();
    if quiet_min > fleet::STALENESS_MINUTES {
        Some(format!("background · quiet {quiet_min}m"))
    } else {
        Some("background".to_string())
    }
}

/// Visible label for a tab waiting on its own timer: when it picks back up
/// (local time), so a quiet card doesn't read as idle.
fn waiting_label(status: &FleetStatus, wake_at: Option<DateTime<Utc>>) -> Option<String> {
    if *status != FleetStatus::Waiting {
        return None;
    }
    Some(match wake_at {
        Some(w) => format!("timer · {}", w.with_timezone(&chrono::Local).format("%-I:%M %p")),
        None => "timer".to_string(),
    })
}

/// The three state groups the Fleet list is organised by. State, not origin:
/// a Hobbes tab that needs you sits beside a terminal session that does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    NeedsYou,
    Working,
    Idle,
}

impl Group {
    const ALL: [Group; 3] = [Group::NeedsYou, Group::Working, Group::Idle];

    fn label(self) -> &'static str {
        match self {
            Group::NeedsYou => "Needs you",
            Group::Working => "Working",
            Group::Idle => "Idle",
        }
    }
}

/// Working covers background phases and timer waits: the session is still
/// on the job and nothing is asked of the user.
fn group_of(status: &FleetStatus) -> Group {
    match status {
        FleetStatus::NeedsAttention(_) => Group::NeedsYou,
        FleetStatus::Working | FleetStatus::WorkingBackground | FleetStatus::Waiting => {
            Group::Working
        }
        FleetStatus::Idle => Group::Idle,
    }
}

/// Live sessions bucketed by [`Group`] (indexed `Group as usize`), each
/// bucket most recent activity first — except that idle rows whose brief
/// says they are blocked lead the Idle group.
fn grouped_sessions(state: &fleet::FleetState) -> [Vec<FleetSession>; 3] {
    let mut groups: [Vec<FleetSession>; 3] = Default::default();
    for s in state.sessions.values() {
        groups[group_of(&s.status) as usize].push(s.clone());
    }
    for (i, g) in groups.iter_mut().enumerate() {
        let blocked_first = i == Group::Idle as usize;
        g.sort_by(|a, b| {
            let ord = if blocked_first {
                blocked_text(b).is_some().cmp(&blocked_text(a).is_some())
            } else {
                std::cmp::Ordering::Equal
            };
            ord.then_with(|| b.last_event_at.cmp(&a.last_event_at))
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    groups
}

/// The brief's blocker, shown whenever the session is not on the job (no
/// turn, background phase or timer running): a session doing work is not
/// blocked-waiting on the user, whatever an older brief believed.
fn blocked_text(session: &FleetSession) -> Option<String> {
    if session.status.is_active() {
        return None;
    }
    session
        .brief
        .as_ref()
        .and_then(|b| b.blocked_on.as_deref())
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
}

/// The brief's headline, display-safe: briefs stored before headlines were
/// cleaned may hold raw JSON, which is unwrapped or dropped.
fn brief_headline(session: &FleetSession) -> Option<String> {
    session.brief.as_ref().and_then(|b| fleet::briefs::clean_headline(&b.headline))
}

/// Idle rows shown before the rest fold behind a toggle.
const IDLE_VISIBLE: usize = 3;

/// (rows shown, rows folded away) for the idle group.
fn idle_split(total: usize, expanded: bool) -> (usize, usize) {
    if expanded || total <= IDLE_VISIBLE {
        (total, 0)
    } else {
        (IDLE_VISIBLE, total - IDLE_VISIBLE)
    }
}

/// The header's one sentence as (amber needs-you part, muted rest):
/// "1 needs you" · "3 working · 4h 10m agent time today".
fn summary_parts(needs_you: usize, working: usize, agent_minutes: u32) -> (Option<String>, String) {
    let needs = match needs_you {
        0 => None,
        1 => Some("1 needs you".to_string()),
        n => Some(format!("{n} need you")),
    };
    let mut rest = Vec::new();
    if working > 0 {
        rest.push(format!("{working} working"));
    }
    if agent_minutes > 0 {
        rest.push(format!("{} agent time today", format_minutes(agent_minutes)));
    }
    let rest = if rest.is_empty() && needs.is_none() {
        "Nothing running".to_string()
    } else {
        rest.join(" · ")
    };
    (needs, rest)
}

/// The row's one status phrase, by priority: a pending gate's tool and
/// input, the notification message, the brief's blocker (when the session
/// isn't running), the brief headline, the background/timer label, then "—".
fn status_phrase(session: &FleetSession, now: DateTime<Utc>) -> String {
    if let Some(gate) = &session.pending_gate {
        return format!("{}: {}", gate.tool_name, gate.input_summary);
    }
    if let FleetStatus::NeedsAttention(AttentionKind::Notification { message, .. }) = &session.status {
        if !message.is_empty() {
            return message.clone();
        }
    }
    if let Some(b) = blocked_text(session) {
        return format!("blocked: {b}");
    }
    if let Some(h) = brief_headline(session) {
        return h;
    }
    background_label(&session.status, session.last_event_at, now)
        .or_else(|| waiting_label(&session.status, session.wake_at))
        .unwrap_or_else(|| "—".to_string())
}

/// `/Users/me/Sites/x` -> `~/Sites/x` when under `home`.
fn shorten_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty()) else {
        return path.to_string();
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// The accessible/tooltip label for a row's go-to action.
fn go_label(session: &FleetSession) -> String {
    if session.origin == fleet::FleetOrigin::Hobbes {
        return "Open chat tab".to_string();
    }
    let l = fleet::terminal_focus::destination_label(session);
    if l.is_empty() { "Go to session".to_string() } else { l }
}

/// The amber primary button's label on a needs-you row.
fn answer_button_label(session: &FleetSession) -> String {
    if session.origin == fleet::FleetOrigin::Hobbes {
        return "Answer in chat ↗".to_string();
    }
    let l = fleet::terminal_focus::answer_label(session);
    let l = if l.is_empty() { "Answer in terminal".to_string() } else { l };
    if l.ends_with('↗') { l } else { format!("{l} ↗") }
}

/// Wide-window layout: the Fleet root is a CSS size container, and past this
/// width the state groups sit side by side as columns of compact cards (the
/// app's responsive idiom is container queries, see `.tab-item` in
/// main.css — no window-size polling). Rendered once, restyled by CSS, so
/// expand/note state survives a resize.
const FLEET_CSS: &str = r#"
.fleet-root { container-type: inline-size; container-name: fleet; }
.fleet-group-empty { display: none; }
.fleet-name { flex: 0 1 auto; max-width: 20rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.fleet-project { flex: 0 1 auto; max-width: 14rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.fleet-phrase { flex: 1 1 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
@container fleet (min-width: 1600px) {
  .fleet-groups { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 20px; align-items: start; }
  .fleet-group-empty { display: block; }
  .fleet-row-line { flex-wrap: wrap; padding-top: 10px; padding-bottom: 10px; }
  .fleet-project { flex: 1 1 0; max-width: none; }
  .fleet-phrase { flex: 1 1 100%; order: 1; white-space: normal; overflow: visible; line-height: 1.45; }
}
"#;

#[component]
pub fn FleetView() -> Element {
    let fleet_state = use_context::<Signal<fleet::FleetState>>();
    let settings = use_context::<Signal<crate::settings::Settings>>();
    let mut idle_open = use_signal(|| false);

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
    let groups = grouped_sessions(&state);
    let live_count: usize = groups.iter().map(Vec::len).sum();
    let agent_minutes = fleet::fleet_agent_minutes_on(&state, today, now);
    let engaged_minutes = fleet::engaged_seconds_on(&state, today, now) / 60;
    let ended_today: Vec<FleetSession> = fleet::store::sessions_active_on(today)
        .into_iter()
        .filter(|r| !state.sessions.contains_key(&r.id))
        .collect();
    let enabled = settings.read().fleet_enabled;
    let connected = fleet::hooks_config::claude_settings_path()
        .and_then(|p| fleet::hooks_config::connected_port_file(&p))
        .is_some();
    drop(state);
    let (needs_part, rest_part) = summary_parts(
        groups[Group::NeedsYou as usize].len(),
        groups[Group::Working as usize].len(),
        agent_minutes,
    );

    rsx! {
        style { "{FLEET_CSS}" }
        div {
            class: "fleet-root flex-1 min-w-0 flex flex-col overflow-y-auto",

            // Header: identity, listener state, one muted sentence. Amber
            // only when something actually needs the user.
            div {
                class: "px-6 pt-6 pb-3 flex items-center gap-3 flex-wrap",
                Icon {
                    icon: crate::components::pixel_icons::HobbesTiger,
                    width: 22,
                    height: 22,
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
                p {
                    class: "text-xs text-fg-muted",
                    if let Some(n) = &needs_part {
                        span { class: "font-semibold", style: "color: #f59e0b;", "{n}" }
                        if !rest_part.is_empty() {
                            " · "
                        }
                    }
                    "{rest_part}"
                }
            }

            if crate::heartbeat::heartbeat_available() {
                crate::components::heartbeat_card::HeartbeatCard { now }
            }

            if live_count == 0 {
                div {
                    class: "px-6 py-8 text-sm text-fg-muted space-y-2",
                    p { "No Claude Code sessions observed yet." }
                    if !enabled {
                        p { "Turn on the fleet in Settings → Fleet, then Connect Claude Code." }
                    } else if !connected {
                        p { "Hooks aren't registered yet — open Settings → Fleet and hit \"Connect Claude Code\"." }
                    } else {
                        p { "Hooks are connected. Sessions appear here the moment one starts (or fires its next event)." }
                    }
                }
            } else {
                // One list grouped by state; side-by-side columns when the
                // Fleet itself is wide (see FLEET_CSS).
                div {
                    class: "fleet-groups px-6 pt-3",
                    for group in Group::ALL {
                        {
                            let rows = groups[group as usize].clone();
                            let total = rows.len();
                            let (shown, hidden) = if group == Group::Idle {
                                idle_split(total, *idle_open.read())
                            } else {
                                (total, 0)
                            };
                            let folded = group == Group::Idle && (hidden > 0 || (*idle_open.read() && total > IDLE_VISIBLE));
                            rsx! {
                                div {
                                    key: "{group.label()}",
                                    class: if total == 0 { "fleet-group-empty mb-4" } else { "mb-4" },
                                    p {
                                        class: "text-[11px] uppercase tracking-wider text-fg-muted mb-2",
                                        { format!("{} — {}", group.label(), total) }
                                    }
                                    div {
                                        class: if group == Group::Idle { "flex flex-col gap-2 opacity-60" } else { "flex flex-col gap-2" },
                                        for session in rows.into_iter().take(shown) {
                                            FleetRow { key: "{session.id}", session: session.clone(), now, today }
                                        }
                                    }
                                    if folded {
                                        button {
                                            class: "mt-2 text-xs text-fg-muted hover:text-fg transition-colors select-none",
                                            onclick: move |_| { let v = !*idle_open.peek(); idle_open.set(v); },
                                            if hidden > 0 {
                                                { format!("▸ {hidden} more idle") }
                                            } else {
                                                "▾ Show fewer"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            EarlierToday { rows: ended_today, now, today }

            ReviewBand { now, today, agent_minutes, engaged_minutes }

            div { class: "pb-16" }
        }
    }
}

/// Ended sessions, out of the way: one collapsed line summarizing them,
/// expandable into a compact ledger. Replaces the old duplicate per-session
/// list in the header card.
#[component]
fn EarlierToday(rows: Vec<FleetSession>, now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
    let mut open = use_signal(|| false);
    let session_state = use_context::<Signal<crate::session::SessionState>>();
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
                                { row_label(s, &session_state.peek()).0 }
                            }
                            span { class: "text-fg-muted shrink-0",
                                { crate::todo::model::format_minutes(s.minutes_on(today, now)) } }
                            if let Some(h) = brief_headline(s) {
                                span { class: "text-fg-muted truncate",
                                    { fleet::truncate_summary(&h, 120) } }
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
fn ReviewBand(
    now: DateTime<Utc>,
    today: chrono::NaiveDate,
    agent_minutes: u32,
    engaged_minutes: u32,
) -> Element {
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
    use_effect(move || {
        let d = viewing();
        rollup.set(load_rollup(d));
    });
    // Pick up a rollup that finished (possibly while this view was
    // unmounted, since `on_rollup` survives navigation via `spawn_forever`)
    // and stage it into the locally-displayed signals.
    use_effect(move || {
        // `take_fresh_rollup` both subscribes this effect to the global
        // (only a `.read()` does that) and clears the slot, without ever
        // holding a read guard across the write.
        if let Some(r) = take_fresh_rollup() {
            viewing.set(r.date);
            rollup.set(Some(r));
        }
    });

    let on_rollup = move |_| {
        if *FLEET_ROLLUP_RUNNING.peek() {
            return;
        }
        *FLEET_ROLLUP_RUNNING.write() = true;
        *FLEET_ROLLUP_ERROR.write() = None;
        spawn_forever(async move {
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
                    // This task is root-owned (`spawn_forever`) and may
                    // outlive the `FleetView` that started it — don't touch
                    // its local `viewing`/`rollup` signals directly (they'd
                    // be dropped along with the component if the user
                    // navigated away). Stage the result globally instead;
                    // the effect above picks it up on whichever mount is
                    // current, including a fresh one on re-entry.
                    *FLEET_ROLLUP_FRESH.write() = Some(r);
                }
                Err(e) => *FLEET_ROLLUP_ERROR.write() = Some(e),
            }
            *FLEET_ROLLUP_RUNNING.write() = false;
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
            class: "px-6 pt-3 pb-1",
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
                    ExocortexFigure { agent_minutes, engaged_minutes }
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
                        disabled: *FLEET_ROLLUP_RUNNING.read(),
                        onclick: on_rollup,
                        if *FLEET_ROLLUP_RUNNING.read() { "Rolling up…" } else { "Roll up my day" }
                    }
                }
                if *open.read() {
                    if let Some(r) = &current {
                        p { class: "mt-2 text-sm text-fg leading-relaxed", "{r.narrative}" }
                    } else {
                        p { class: "mt-2 text-xs text-fg-muted", "Roll up your day to get a short review of what every session got done, what's blocked, and what to pick up next." }
                    }
                    BoostRibbon { date: viewing_day, today, now }
                }
                if let Some(err) = FLEET_ROLLUP_ERROR.read().clone() {
                    p { class: "mt-2 text-xs text-red-400", "{err}" }
                }
            }
        }
    }
}

/// The exocortex multiplier: agent minutes over ENGAGED minutes (wall-clock
/// with >=1 agent working) — average concurrency while driving the fleet.
/// Breaks, planning, and walks don't decay it; parallel agents raise it.
/// Dimmed rather than removed until there is enough signal.
#[component]
fn ExocortexFigure(agent_minutes: u32, engaged_minutes: u32) -> Element {
    match fleet::exocortex_multiplier(agent_minutes, engaged_minutes) {
        Some((mult, engaged)) => rsx! {
            div {
                class: "flex items-baseline gap-1.5 shrink-0",
                title: format!(
                    "{} of agent work across {} engaged — average concurrency while you're driving the fleet (the ribbon's peak is the high-water mark)",
                    format_minutes(agent_minutes),
                    format_minutes(engaged),
                ),
                Icon { width: 14, height: 14, icon: fi_icons::FiZap, class: "self-center", style: "color: #a78bfa;" }
                span {
                    class: "text-sm font-semibold tabular-nums",
                    style: "color: #a78bfa;",
                    { format!("{:.1}×", mult) }
                }
                span { class: "text-[11px] uppercase tracking-wider text-fg-muted", "exocortex" }
            }
        },
        None => rsx! {
            div {
                class: "flex items-baseline gap-1.5 shrink-0",
                title: "Shows once the fleet has agent work and ≥5 engaged minutes today",
                Icon { width: 14, height: 14, icon: fi_icons::FiZap, class: "self-center opacity-40", style: "color: #a78bfa;" }
                span { class: "text-sm font-semibold tabular-nums text-fg-muted opacity-60", "—×" }
                span { class: "text-[11px] uppercase tracking-wider text-fg-muted", "exocortex" }
            }
        },
    }
}

// ── Boost ribbon ────────────────────────────────────────────────────────────

/// The exocortex identity color (the header multiplier's violet). The ribbon
/// is that stat's intra-day view, so it wears the same hue — in two strength
/// steps, because driven vs background is one measure at two intensities,
/// not two categories (the status green/teal pair fails CVD separation as
/// adjacent area fills; a single hue can't).
const BOOST_COLOR: &str = "#a78bfa";

/// Ribbon viewBox height and the headroom above the tallest bucket.
const RIBBON_H: f32 = 44.0;
const RIBBON_TOP_PAD: f32 = 6.0;

/// Closed SVG polygon (`points` attr) for an area drawn through bucket
/// centers and squared off to the baseline at both ends. `upto` trims the
/// future portion of a live day; x is one viewBox unit per bucket.
fn area_points(values: &[f32], upto: usize, max: f32) -> String {
    let upto = upto.min(values.len());
    if upto == 0 || max <= 0.0 {
        return String::new();
    }
    let y = |v: f32| RIBBON_H - (v.max(0.0) / max) * (RIBBON_H - RIBBON_TOP_PAD);
    let mut pts = Vec::with_capacity(upto + 4);
    pts.push(format!("0,{RIBBON_H}"));
    pts.push(format!("0,{:.2}", y(values[0])));
    for (i, v) in values[..upto].iter().enumerate() {
        pts.push(format!("{:.1},{:.2}", i as f32 + 0.5, y(*v)));
    }
    pts.push(format!("{upto},{:.2}", y(values[upto - 1])));
    pts.push(format!("{upto},{RIBBON_H}"));
    pts.join(" ")
}

/// The honesty line on the ribbon's tooltip: replayed minutes against the banked
/// truth. The reconstruction is boundary-events-only, so the two legitimately
/// differ — the caption quantifies by how much instead of implying exactness.
fn fidelity_line(replayed: u32, banked: u32) -> Option<String> {
    if replayed == 0 && banked == 0 {
        return None;
    }
    let pct = (replayed.min(banked) * 100) / replayed.max(banked);
    Some(format!(
        "replayed {} of {} banked agent time · {}% match",
        format_minutes(replayed),
        format_minutes(banked),
        pct
    ))
}

/// Native-tooltip text for one bucket: time window, total boost, the split.
fn bucket_tip(i: usize, bucket_minutes: u32, driven: f32, total: f32) -> String {
    let m0 = i as u32 * bucket_minutes;
    let m1 = (m0 + bucket_minutes).min(24 * 60);
    let hm = |m: u32| format!("{}:{:02}", m / 60, m % 60);
    format!(
        "{}–{} · {:.1}× — {:.1} driven + {:.1} background",
        hm(m0),
        hm(m1),
        total,
        driven,
        (total - driven).max(0.0)
    )
}

/// The boost ribbon: the exocortex through the day, replayed from the event
/// log as a stacked area — solid violet is turns you drove, the haze above
/// it is background work grinding alongside. Follows the review band's
/// viewed day; today's strip ends at now instead of drawing hours that
/// haven't happened. Hidden when the day has no events (nothing to show).
#[component]
fn BoostRibbon(date: chrono::NaiveDate, today: chrono::NaiveDate, now: DateTime<Utc>) -> Element {
    const BUCKET_MIN: u32 = 15;
    let fleet_state = use_context::<Signal<fleet::FleetState>>();

    let events = fleet::store::events_on(date);
    if events.is_empty() {
        return rsx! {};
    }
    let (day_start, day_end) = fleet::local_day_bounds_utc(date);
    let bound = if date == today { now } else { day_end };
    let spans = fleet::replay::replay_day_spans(&events, bound);
    let total = fleet::replay::concurrency_curve(&spans, date, BUCKET_MIN);
    let driven_spans: Vec<fleet::replay::DaySpan> =
        spans.iter().filter(|s| !s.background).cloned().collect();
    let driven = fleet::replay::concurrency_curve(&driven_spans, date, BUCKET_MIN);
    let upto = if date == today {
        let elapsed = (now - day_start).num_minutes().clamp(0, 24 * 60) as u32;
        (elapsed.div_ceil(BUCKET_MIN) as usize).min(total.len())
    } else {
        total.len()
    };
    let peak = total[..upto].iter().copied().fold(0f32, f32::max);
    if peak <= 0.0 {
        return rsx! {};
    }
    let max = peak.max(1.0);
    let total_pts = area_points(&total, upto, max);
    let driven_pts = area_points(&driven, upto, max);
    let n = total.len();
    let one_y = RIBBON_H - (1.0 / max) * (RIBBON_H - RIBBON_TOP_PAD);

    let replayed = fleet::replay::replayed_minutes_on(&spans, date);
    let banked = fleet::fleet_agent_minutes_on(&fleet_state.read(), date, now);
    let caption = fidelity_line(replayed, banked);
    let tips: Vec<(usize, String)> = (0..upto)
        .filter(|&i| total[i] > 0.05)
        .map(|i| (i, bucket_tip(i, BUCKET_MIN, driven[i], total[i])))
        .collect();

    rsx! {
        div {
            class: "mt-3",
            title: caption.clone().unwrap_or_default(),
            div {
                class: "flex items-baseline gap-2",
                span {
                    class: "text-[11px] font-semibold",
                    style: "color: {BOOST_COLOR};",
                    "Boost through the day"
                }
                if peak >= 1.05 {
                    span {
                        class: "text-[11px] tabular-nums text-fg-muted",
                        { format!("peak {:.1}×", peak) }
                    }
                }
                div { class: "flex-1" }
                // One hue, two strengths — the legend teaches the split.
                span { class: "w-2 h-2 rounded-sm self-center", style: "background: {BOOST_COLOR};" }
                span { class: "text-[10px] text-fg-muted", "driven" }
                span {
                    class: "w-2 h-2 rounded-sm self-center ml-1",
                    style: "background: {BOOST_COLOR}; opacity: 0.32;",
                }
                span { class: "text-[10px] text-fg-muted", "background" }
            }
            svg {
                class: "w-full mt-1 rounded-sm",
                style: "background: rgba(255,255,255,0.02); display: block;",
                height: "44",
                view_box: "0 0 {n} 44",
                preserve_aspect_ratio: "none",
                // Quarter-day ticks (6a / 12p / 6p): thin rects instead of
                // strokes — stroke widths distort under the stretched
                // viewBox, sub-unit rects stay ≈1px.
                for x in [n / 4, n / 2, 3 * n / 4] {
                    rect {
                        x: "{x}",
                        y: "0",
                        width: "0.12",
                        height: "{RIBBON_H}",
                        fill: "rgba(255,255,255,0.06)",
                    }
                }
                // The 1× guide: above this line the fleet outnumbers you.
                if max > 1.2 {
                    rect {
                        x: "0",
                        y: "{one_y}",
                        width: "{n}",
                        height: "0.4",
                        fill: "rgba(255,255,255,0.10)",
                    }
                }
                polygon { points: "{total_pts}", fill: "{BOOST_COLOR}", opacity: "0.30" }
                polygon { points: "{driven_pts}", fill: "{BOOST_COLOR}", opacity: "0.85" }
                // Invisible per-bucket hit targets carrying native tooltips.
                for (i, tip) in tips {
                    rect {
                        x: "{i}",
                        y: "0",
                        width: "1",
                        height: "{RIBBON_H}",
                        fill: "transparent",
                        title { "{tip}" }
                    }
                }
            }
            div {
                class: "flex justify-between text-[10px] text-fg-muted/70 mt-0.5",
                span { "12a" }
                span { "6a" }
                span { "12p" }
                span { "6p" }
                span { "12a" }
            }
        }
    }
}

/// Show a one-line note under a row; it fades after ~4s (a newer note
/// outlives an older one's timer).
fn flash_note(mut note: Signal<Option<(i64, String)>>, text: String) {
    let stamp = Utc::now().timestamp_millis();
    note.set(Some((stamp, text)));
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        if note.peek().as_ref().map(|n| n.0) == Some(stamp) {
            note.set(None);
        }
    });
}

/// Shown when Approve / Deny reaches a request that is no longer held.
const GATE_GONE_NOTE: &str = "That request is no longer waiting \u{2014} it timed out or the run moved on.";

/// Go to the session: a Hobbes tab switches (which also dismisses the fleet
/// view); a Claude Code session walks the terminal-focus ladder and reports
/// where it ended up.
fn go_to(
    session: FleetSession,
    mut chat_command: Signal<Option<crate::components::chat_input::ChatCommand>>,
    note: Signal<Option<(i64, String)>>,
) {
    if session.origin == fleet::FleetOrigin::Hobbes {
        chat_command.set(Some(
            crate::components::chat_input::ChatCommand::SwitchToSession(session.id.clone()),
        ));
        return;
    }
    let name = session.display_name().to_string();
    spawn(async move {
        let outcome = crate::fleet::terminal_focus::go_to_session(session).await;
        let text = outcome.note(&name);
        if !text.is_empty() {
            flash_note(note, text);
        }
    });
}

/// A row's label and tab emoji. A Hobbes tab that is open reads its live
/// tab label (the AI's topic or the user's rename) and emoji, so the Fleet
/// matches the tab strip; otherwise the name the row was last reported with.
fn row_label(
    session: &FleetSession,
    session_state: &crate::session::SessionState,
) -> (String, Option<String>) {
    if session.origin == fleet::FleetOrigin::Hobbes {
        if let Some(s) = session_state.sessions.get(&session.id) {
            return (s.display_title(), s.tab_emoji.clone().filter(|e| !e.trim().is_empty()));
        }
    }
    (session.display_name().to_string(), None)
}

#[component]
fn FleetRow(session: FleetSession, now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
    let chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let planner = use_context::<Signal<crate::todo::PlannerState>>();
    let session_state = use_context::<Signal<crate::session::SessionState>>();
    let mut expanded = use_signal(|| false);
    let note = use_signal(|| Option::<(i64, String)>::None);

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
    // A finished turn at Claude Code's idle prompt asks nothing: it reads
    // as idle, not "needs you" (`FleetSession::asks_person`).
    let attention = session.asks_person();
    let (chip_label, chip_color) = if session.status.needs_attention() && !attention {
        status_chip(&FleetStatus::Idle)
    } else {
        status_chip(&session.status)
    };
    // Attention rows get a loud left edge; computed color → inline style.
    let row_style = if attention {
        format!("border-left: 3px solid {chip_color}; background: {chip_color}14;")
    } else {
        "border-left: 3px solid transparent;".to_string()
    };
    let is_hobbes = session.origin == fleet::FleetOrigin::Hobbes;
    // Memoised: session_state changes on every streamed chunk, but the row
    // only re-renders when its own label or emoji does.
    let label_session = session.clone();
    let label = use_memo(move || row_label(&label_session, &session_state.read()));
    let (row_name, row_emoji) = label();
    let phrase = status_phrase(&session, now);
    let phrase_blocked = session.pending_gate.is_none()
        && blocked_text(&session).is_some_and(|b| phrase == format!("blocked: {b}"));
    let phrase_style = if phrase_blocked { "color: #f59e0b;" } else { "" };
    // A headless run Hobbes dispatched has no terminal: its permission
    // request is held for Approve / Deny right here.
    let held_gate = session.held_gate().map(|g| g.request_id.clone());
    let headless_live = session.is_headless_dispatch() && session.ended_at.is_none();
    let go_title = go_label(&session);
    let is_open = *expanded.read();
    let note_text = note.read().as_ref().map(|n| n.1.clone());
    let automation_denied = note_text.is_some() && fleet::terminal_focus::automation_denied_for(&session);

    let (row_go, btn_go, answer_go, resume_s) = (
        session.clone(),
        session.clone(),
        session.clone(),
        session.clone(),
    );
    let dismiss_id = session.id.clone();
    let row_dismiss_id = session.id.clone();

    rsx! {
        div {
            class: "group rounded border border-subtle bg-section",
            style: "{row_style}",
            div {
                class: "fleet-row-line min-h-11 px-3 py-1.5 flex items-center gap-2 cursor-pointer hover:bg-card transition-colors",
                onclick: move |_| go_to(row_go.clone(), chat_command, note),
                button {
                    class: "shrink-0 w-4 text-[11px] text-fg-muted hover:text-fg select-none",
                    title: if is_open { "Collapse" } else { "Expand" },
                    aria_label: if is_open { "Collapse" } else { "Expand" },
                    aria_expanded: "{is_open}",
                    onclick: move |evt| {
                        evt.stop_propagation();
                        let v = !*expanded.peek();
                        expanded.set(v);
                    },
                    if is_open { "▾" } else { "▸" }
                }
                if let Some(e) = &row_emoji {
                    span { class: "shrink-0 text-[14px] leading-none w-4 text-center", "{e}" }
                } else if is_hobbes {
                    Icon { width: 16, height: 16, icon: fi_icons::FiMessageSquare, class: "shrink-0" }
                } else {
                    Icon { width: 16, height: 16, icon: fi_icons::FiTerminal, class: "shrink-0" }
                }
                span { class: "fleet-name text-sm font-semibold", "{row_name}" }
                span {
                    class: "fleet-project text-xs text-fg-muted",
                    { project.clone().unwrap_or_default() }
                }
                // Status as a dot, not a pill — the full name of the state
                // lives in the tooltip; attention pulses.
                span {
                    class: if attention {
                        "w-2.5 h-2.5 rounded-full shrink-0 animate-pulse"
                    } else {
                        "w-2.5 h-2.5 rounded-full shrink-0"
                    },
                    style: "background-color: {chip_color};",
                    title: "{chip_label}",
                }
                span { class: "fleet-phrase text-xs text-fg", style: "{phrase_style}", title: "{phrase}", "{phrase}" }
                span {
                    class: "shrink-0 text-[11px] text-fg-muted tabular-nums",
                    { age_label(session.last_event_at, now) }
                }
                if let Some(rid) = held_gate.clone() {
                    button {
                        class: "shrink-0 px-3 py-1 text-xs font-medium rounded min-h-8",
                        style: "border: 1px solid #f59e0b; background: rgba(245,158,11,0.12); color: #fcd34d;",
                        title: "Let the headless run use this tool",
                        onclick: {
                            let rid = rid.clone();
                            move |evt: MouseEvent| {
                                evt.stop_propagation();
                                if !fleet::shared().resolve_gate(&rid, true) {
                                    flash_note(note, GATE_GONE_NOTE.to_string());
                                }
                            }
                        },
                        "Approve"
                    }
                    button {
                        class: "shrink-0 px-3 py-1 text-xs font-medium rounded min-h-8 border border-subtle text-fg-muted hover:text-fg hover:bg-card",
                        title: "Refuse this tool; the run carries on without it",
                        onclick: move |evt: MouseEvent| {
                            evt.stop_propagation();
                            if !fleet::shared().resolve_gate(&rid, false) {
                                flash_note(note, GATE_GONE_NOTE.to_string());
                            }
                        },
                        "Deny"
                    }
                } else if attention {
                    button {
                        class: "shrink-0 px-3 py-1 text-xs font-medium rounded min-h-8",
                        style: "border: 1px solid #f59e0b; background: rgba(245,158,11,0.12); color: #fcd34d;",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            go_to(answer_go.clone(), chat_command, note);
                        },
                        { answer_button_label(&session) }
                    }
                } else if !headless_live {
                    button {
                        class: "shrink-0 px-1.5 py-1 text-sm text-fg-muted hover:text-fg opacity-60 group-hover:opacity-100 focus:opacity-100 transition-opacity",
                        title: "{go_title}",
                        aria_label: "{go_title}",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            go_to(btn_go.clone(), chat_command, note);
                        },
                        "↗"
                    }
                }
                // Hide, hover-revealed like the tab-close idiom (as before the
                // redesign): the escape hatch for a row whose terminal went
                // away without a SessionEnd. External only: Hobbes tabs leave
                // the board when they close. Also in the expanded details.
                if !is_hobbes {
                    button {
                        class: "shrink-0 px-1 py-1 text-fg-muted hover:text-fg opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity",
                        title: "Hide this session from the fleet (it comes back on its next event)",
                        aria_label: "Hide this session from the fleet",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            fleet::shared().dismiss_session(&row_dismiss_id);
                        },
                        Icon { width: 14, height: 14, icon: fi_icons::FiX }
                    }
                }
            }

            if let Some(text) = note_text {
                p {
                    class: "px-3 pb-1.5 text-[11px] text-fg-muted",
                    role: "status",
                    "{text}"
                    if automation_denied {
                        " "
                        button {
                            class: "text-primary-300 hover:text-fg underline",
                            onclick: move |_| {
                                if let Err(e) = open::that_detached(fleet::terminal_focus::AUTOMATION_SETTINGS_URL) {
                                    tracing::warn!("Couldn't open System Settings: {e}");
                                }
                            },
                            "Open Settings"
                        }
                    }
                }
            }

            if is_open {
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
                    let home = dirs::home_dir().map(|h| h.to_string_lossy().into_owned());
                    let mut facts: Vec<String> = Vec::new();
                    if is_hobbes {
                        facts.push("Hobbes chat tab".to_string());
                    } else {
                        facts.push(shorten_home(&session.cwd, home.as_deref()));
                        if let Some(t) = fleet::terminal_focus::terminal_name(&session) {
                            facts.push(t);
                        }
                    }
                    let minutes_today = session.minutes_on(today, now);
                    if minutes_today > 0 {
                        facts.push(format!("{} today", format_minutes(minutes_today)));
                    }
                    if let Some(t) = session.working_since {
                        facts.push(format!(
                            "started {}",
                            t.with_timezone(&Local).format("%-I:%M %p")
                        ));
                    }
                    let facts = facts.join(" · ");
                    let resume = fleet::terminal_focus::resume_command(&resume_s);
                    let blocked = blocked_text(&session);
                    let headline = brief_headline(&session);
                    rsx! {
                        div {
                            class: "mx-3 mb-3 rounded bg-input p-2 space-y-1",
                            if let Some(brief) = &session.brief {
                                if let Some(h) = headline {
                                    p { class: "text-xs text-fg", "{h}" }
                                }
                                for b in brief.bullets.iter().take(3) {
                                    p { class: "text-xs text-fg-muted", { format!("• {b}") } }
                                }
                            }
                            if let Some(blocked) = blocked {
                                p {
                                    class: "text-xs",
                                    style: "color: #f59e0b;",
                                    { format!("blocked: {}", fleet::truncate_summary(&blocked, 140)) }
                                }
                            }
                            if let Some(title) = working_on {
                                p {
                                    class: "text-xs text-fg-muted",
                                    span { class: "text-fg-muted/70", "working on: " }
                                    span { class: "text-fg", { fleet::truncate_summary(&title, 120) } }
                                }
                            }
                            p { class: "text-xs text-fg-muted break-all", "{facts}" }
                            if session.is_headless_dispatch() {
                                p { class: "text-[11px] text-fg-muted",
                                    "Headless run launched by Hobbes for a todo \u{2014} no terminal; approve or deny its permission requests here (unanswered for about two minutes, a request is refused)."
                                }
                            } else if session.dispatched_todo.is_some() {
                                p { class: "text-[11px] text-fg-muted", "Launched by Hobbes for a todo" }
                            }
                            div {
                                class: "pt-1 flex items-center gap-2 flex-wrap",
                                if !headless_live {
                                    button {
                                        class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card",
                                        onclick: {
                                            let s = session.clone();
                                            move |_| go_to(s.clone(), chat_command, note)
                                        },
                                        { format!("{} ↗", go_title) }
                                    }
                                }
                                if let Some(cmd) = resume {
                                    button {
                                        class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card",
                                        onclick: move |_| {
                                            match feature_clipboard::copy_to_clipboard(&cmd) {
                                                Ok(()) => flash_note(note, "Resume command copied".to_string()),
                                                Err(e) => flash_note(note, format!("Couldn't copy: {e}")),
                                            }
                                        },
                                        "Copy resume command"
                                    }
                                }
                                div { class: "flex-1" }
                                // The escape hatch for a card whose terminal went
                                // away without a SessionEnd (the sweeps never
                                // retire needs-attention rows). External only —
                                // Hobbes tabs leave the board when they close.
                                if !is_hobbes {
                                    button {
                                        class: "text-xs px-2 py-1 text-fg-muted hover:text-fg",
                                        title: "Hide this session from the fleet (it comes back on its next event)",
                                        onclick: move |_| fleet::shared().dismiss_session(&dismiss_id),
                                        "Hide"
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
    fn background_label_spells_out_quiet_age() {
        let now = utc("2026-09-04T12:00:00Z");
        let fresh = now - chrono::Duration::minutes(2);
        let quiet = now - chrono::Duration::minutes(25);
        assert_eq!(
            background_label(&FleetStatus::WorkingBackground, fresh, now).as_deref(),
            Some("background")
        );
        assert_eq!(
            background_label(&FleetStatus::WorkingBackground, quiet, now).as_deref(),
            Some("background · quiet 25m")
        );
        assert_eq!(background_label(&FleetStatus::Working, quiet, now), None);
        assert_eq!(background_label(&FleetStatus::Idle, quiet, now), None);
    }

    fn session(id: &str, status: FleetStatus, origin: fleet::FleetOrigin, minutes_ago: i64) -> FleetSession {
        let now = utc("2026-08-25T10:00:00Z");
        let ev = crate::fleet::events::FleetEvent::SessionStart {
            session_id: id.into(),
            cwd: format!("/x/{id}"),
            source: "startup".into(),
        };
        let mut st = fleet::FleetState::default();
        fleet::reduce(&mut st, &ev, now);
        let mut s = st.sessions.remove(id).unwrap();
        s.status = status;
        s.origin = origin;
        s.last_event_at = now - chrono::Duration::minutes(minutes_ago);
        s
    }

    fn notif() -> FleetStatus {
        FleetStatus::NeedsAttention(AttentionKind::Notification {
            kind: "permission_prompt".into(),
            message: "Allow Bash?".into(),
        })
    }

    #[test]
    fn groups_by_state_not_origin_and_attention_leads() {
        // The old origin grouping buried a Hobbes tab that needed you.
        let mut state = fleet::FleetState::default();
        for s in [
            session("idle-old", FleetStatus::Idle, fleet::FleetOrigin::External, 50),
            session("work-fresh", FleetStatus::Working, fleet::FleetOrigin::External, 1),
            session("tab-needs", notif(), fleet::FleetOrigin::Hobbes, 40),
            session("cc-gate", FleetStatus::NeedsAttention(AttentionKind::Gate), fleet::FleetOrigin::External, 5),
            session("bg", FleetStatus::WorkingBackground, fleet::FleetOrigin::Hobbes, 9),
            session("timer", FleetStatus::Waiting, fleet::FleetOrigin::Hobbes, 3),
        ] {
            state.sessions.insert(s.id.clone(), s);
        }
        let g = grouped_sessions(&state);
        let ids = |grp: Group| -> Vec<&str> { g[grp as usize].iter().map(|s| s.id.as_str()).collect() };
        assert_eq!(ids(Group::NeedsYou), vec!["cc-gate", "tab-needs"]);
        assert_eq!(ids(Group::Working), vec!["work-fresh", "timer", "bg"]);
        assert_eq!(ids(Group::Idle), vec!["idle-old"]);
    }

    #[test]
    fn idle_folds_after_three() {
        assert_eq!(idle_split(2, false), (2, 0));
        assert_eq!(idle_split(3, false), (3, 0));
        assert_eq!(idle_split(7, false), (3, 4));
        assert_eq!(idle_split(7, true), (7, 0));
    }

    #[test]
    fn summary_sentence_reads_naturally() {
        assert_eq!(
            summary_parts(1, 3, 250),
            (Some("1 needs you".to_string()), "3 working · 4h 10m agent time today".to_string())
        );
        assert_eq!(summary_parts(2, 0, 0), (Some("2 need you".to_string()), String::new()));
        assert_eq!(summary_parts(0, 2, 0), (None, "2 working".to_string()));
        assert_eq!(summary_parts(0, 0, 0), (None, "Nothing running".to_string()));
    }

    #[test]
    fn status_phrase_priority() {
        let now = utc("2026-08-25T10:00:00Z");
        let mut s = session("a", FleetStatus::Idle, fleet::FleetOrigin::External, 0);
        assert_eq!(status_phrase(&s, now), "—");

        s.status = FleetStatus::Waiting;
        s.wake_at = None;
        assert_eq!(status_phrase(&s, now), "timer");

        s.brief = Some(fleet::SessionBrief {
            headline: "Refactoring the gate resolver".into(),
            bullets: vec![],
            blocked_on: None,
            generated_at: now,
            final_brief: false,
        });
        assert_eq!(status_phrase(&s, now), "Refactoring the gate resolver");

        s.status = notif();
        assert_eq!(status_phrase(&s, now), "Allow Bash?");

        s.pending_gate = Some(fleet::PendingGate {
            request_id: "g".into(),
            tool_name: "Bash".into(),
            input_summary: "rm -rf node_modules".into(),
            requested_at: now,
        });
        assert_eq!(status_phrase(&s, now), "Bash: rm -rf node_modules");
    }

    fn brief(headline: &str, blocked: Option<&str>) -> fleet::SessionBrief {
        fleet::SessionBrief {
            headline: headline.into(),
            bullets: vec![],
            blocked_on: blocked.map(str::to_string),
            generated_at: utc("2026-08-25T10:00:00Z"),
            final_brief: false,
        }
    }

    #[test]
    fn idle_rows_show_their_blocker_ahead_of_the_headline() {
        let now = utc("2026-08-25T10:00:00Z");
        let mut s = session("a", FleetStatus::Idle, fleet::FleetOrigin::External, 0);
        s.brief = Some(brief("Wired the exporter", Some("needs the staging API key")));
        assert_eq!(blocked_text(&s).as_deref(), Some("needs the staging API key"));
        assert_eq!(status_phrase(&s, now), "blocked: needs the staging API key");
        // Needs-you rows are not active either: the blocker still shows.
        s.status = FleetStatus::NeedsAttention(AttentionKind::Gate);
        assert!(blocked_text(&s).is_some());
        // A running session is not blocked, whatever the older brief said.
        for active in [FleetStatus::Working, FleetStatus::WorkingBackground, FleetStatus::Waiting] {
            s.status = active;
            assert_eq!(blocked_text(&s), None);
            assert_eq!(status_phrase(&s, now), "Wired the exporter");
        }
        // Blank blockers are ignored.
        s.status = FleetStatus::Idle;
        s.brief = Some(brief("Wired the exporter", Some("  ")));
        assert_eq!(status_phrase(&s, now), "Wired the exporter");
    }

    #[test]
    fn blocked_idle_rows_lead_the_idle_group() {
        let mut state = fleet::FleetState::default();
        let mut blocked = session("blocked-old", FleetStatus::Idle, fleet::FleetOrigin::External, 90);
        blocked.brief = Some(brief("x", Some("waiting on review")));
        for s in [
            session("fresh", FleetStatus::Idle, fleet::FleetOrigin::External, 1),
            blocked,
            session("older", FleetStatus::Idle, fleet::FleetOrigin::External, 30),
        ] {
            state.sessions.insert(s.id.clone(), s);
        }
        let g = grouped_sessions(&state);
        let ids: Vec<&str> = g[Group::Idle as usize].iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["blocked-old", "fresh", "older"]);
    }

    #[test]
    fn legacy_json_headlines_are_cleaned_for_display() {
        let now = utc("2026-08-25T10:00:00Z");
        let mut s = session("a", FleetStatus::Idle, fleet::FleetOrigin::External, 0);
        s.brief = Some(brief(r#"{"summary": "Fixed the login redirect", "current_task": "tests"}"#, None));
        assert_eq!(brief_headline(&s).as_deref(), Some("Fixed the login redirect"));
        assert_eq!(status_phrase(&s, now), "Fixed the login redirect");
        // Unrecoverable JSON debris is dropped, never shown raw.
        s.brief = Some(brief(r#"{"bullets": ["a"]}"#, None));
        assert_eq!(brief_headline(&s), None);
        assert_eq!(status_phrase(&s, now), "—");
    }

    #[test]
    fn headless_runs_are_answered_in_hobbes_not_a_terminal() {
        let mut run = session("run", FleetStatus::NeedsAttention(AttentionKind::Gate), fleet::FleetOrigin::External, 0);
        run.dispatched_todo = Some("todo-1".into());
        run.pending_gate = Some(fleet::PendingGate {
            request_id: "g".into(),
            tool_name: "Bash".into(),
            input_summary: "npm test".into(),
            requested_at: utc("2026-08-25T10:00:00Z"),
        });
        assert!(run.held_gate().is_some());
        assert!(!answer_button_label(&run).contains("terminal"));
        assert_eq!(go_label(&run), fleet::terminal_focus::HEADLESS_DESTINATION_LABEL);
        // The same request on an interactive session is not held.
        let mut cc = run.clone();
        cc.dispatched_todo = None;
        assert!(cc.held_gate().is_none());
        assert!(answer_button_label(&cc).starts_with("Answer in"));
        // A dispatched run resumed in a real terminal answers there.
        let mut resumed = run.clone();
        resumed.terminal = Some(fleet::terminal_focus::TerminalHint {
            term_program: Some("iTerm.app".into()),
            ..Default::default()
        });
        assert!(!resumed.is_headless_dispatch());
        assert!(resumed.held_gate().is_none());
        // Hobbes's own bundle id alone (inherited by `claude -p`) is not a terminal.
        let mut headless = run.clone();
        headless.terminal = Some(fleet::terminal_focus::TerminalHint {
            bundle_id: Some("ai.clearmirror.cai-hobbes".into()),
            ..Default::default()
        });
        assert!(headless.is_headless_dispatch());
    }

    #[test]
    fn home_is_shortened_to_a_tilde() {
        assert_eq!(shorten_home("/Users/me/Sites/x", Some("/Users/me")), "~/Sites/x");
        assert_eq!(shorten_home("/Users/me", Some("/Users/me/")), "~");
        assert_eq!(shorten_home("/Users/meow/x", Some("/Users/me")), "/Users/meow/x");
        assert_eq!(shorten_home("/opt/x", None), "/opt/x");
    }

    #[test]
    fn hobbes_rows_use_chat_labels() {
        let tab = session("t", notif(), fleet::FleetOrigin::Hobbes, 0);
        assert_eq!(go_label(&tab), "Open chat tab");
        assert_eq!(answer_button_label(&tab), "Answer in chat ↗");
    }

    #[test]
    fn hobbes_rows_show_the_tabs_live_label_and_emoji() {
        let mut state = crate::session::SessionState::default();
        let id = state.create_session_raw(None);
        let mut row = session(&id, FleetStatus::Working, fleet::FleetOrigin::Hobbes, 0);
        row.name = "Oct 22 - 09:15 AM".into();
        if let Some(s) = state.sessions.get_mut(&id) {
            s.tab_title = Some("Vendor research".into());
            s.tab_emoji = Some("🔎".into());
        }
        assert_eq!(row_label(&row, &state), ("Vendor research".to_string(), Some("🔎".to_string())));
        // A closed tab (not hydrated) falls back to the reported name.
        let gone = session("gone", FleetStatus::Idle, fleet::FleetOrigin::Hobbes, 0);
        assert_eq!(row_label(&gone, &state).1, None);
        // Claude Code rows never borrow a tab's label.
        let mut cc = session(&id, FleetStatus::Working, fleet::FleetOrigin::External, 0);
        cc.name = "api-server".into();
        assert_eq!(row_label(&cc, &state).1, None);
    }

    #[test]
    fn area_points_close_to_the_baseline_and_trim_the_future() {
        // Empty and all-future cases render nothing.
        assert_eq!(area_points(&[], 0, 2.0), "");
        assert_eq!(area_points(&[1.0, 1.0], 0, 2.0), "");

        let pts = area_points(&[2.0, 0.0], 2, 2.0);
        let coords: Vec<&str> = pts.split(' ').collect();
        assert_eq!(coords.first(), Some(&"0,44"), "opens on the baseline");
        assert_eq!(coords.last(), Some(&"2,44"), "closes on the baseline");
        // Full-scale value reaches the top pad, zero sits on the baseline.
        assert!(pts.contains("0.5,6.00"), "got: {pts}");
        assert!(pts.contains("1.5,44.00"), "got: {pts}");

        // upto trims: the polygon's right edge is the trim point, not the
        // day's end.
        let trimmed = area_points(&[1.0, 1.0, 1.0, 1.0], 2, 1.0);
        assert!(trimmed.ends_with("2,44"), "got: {trimmed}");
    }

    #[test]
    fn fidelity_line_quantifies_and_hides_the_empty_case() {
        assert_eq!(fidelity_line(0, 0), None);
        let line = fidelity_line(90, 100).unwrap();
        assert!(line.contains("1h 30m") && line.contains("1h 40m"), "got: {line}");
        assert!(line.contains("90% match"), "got: {line}");
        // Over-replay reads the same way — the ratio is symmetric.
        assert!(fidelity_line(100, 90).unwrap().contains("90% match"));
        assert!(fidelity_line(50, 0).unwrap().contains("0% match"));
    }

    #[test]
    fn bucket_tips_name_the_window_and_the_split() {
        let tip = bucket_tip(38, 15, 2.0, 3.2);
        assert!(tip.starts_with("9:30–9:45"), "got: {tip}");
        assert!(tip.contains("3.2×"), "got: {tip}");
        assert!(tip.contains("2.0 driven + 1.2 background"), "got: {tip}");
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

/// Regression tests for the `ReviewBand` fresh-rollup drain effect.
///
/// The bug these pin down: inside a `use_effect`, only `.read()` registers
/// the effect's reactive context. An effect whose body *only* writes a
/// GlobalSignal runs once at mount and is never woken again, so a rollup
/// that finished while Fleet was open stayed invisible until a remount.
#[cfg(test)]
mod fresh_rollup_effect_tests {
    use super::*;
    use dioxus::dioxus_core::NoOpMutations;
    use std::cell::RefCell;

    thread_local! {
        /// One entry per run of the probe effect, holding what it took.
        static SEEN: RefCell<Vec<Option<fleet::briefs::DayRollup>>> =
            const { RefCell::new(Vec::new()) };
    }

    fn rollup(day: &str) -> fleet::briefs::DayRollup {
        fleet::briefs::DayRollup {
            date: day.parse().unwrap(),
            narrative: "narrative".to_string(),
            generated_at: Utc::now(),
            session_count: 1,
            total_minutes: 30,
        }
    }

    /// Mirrors `ReviewBand`'s drain effect, minus the local signals it
    /// stages into: it calls the same helper and records the result.
    #[allow(non_snake_case)]
    fn RollupProbe() -> Element {
        use_effect(|| {
            let fresh = take_fresh_rollup();
            SEEN.with(|s| s.borrow_mut().push(fresh));
        });
        rsx! {}
    }

    /// Pump the dom a bounded number of times so a missed wake-up fails the
    /// assertion instead of hanging the test.
    fn pump(dom: &mut VirtualDom) {
        for _ in 0..8 {
            dom.render_immediate(&mut NoOpMutations);
        }
    }

    #[test]
    fn rollup_staged_after_mount_is_picked_up() {
        let mut dom = VirtualDom::new(RollupProbe);
        crate::test_support::rebuild_or_fail(&mut dom);
        pump(&mut dom);

        assert_eq!(
            SEEN.with(|s| s.borrow().len()),
            1,
            "effect should run once at mount"
        );
        assert!(SEEN.with(|s| s.borrow()[0].is_none()));

        // What the `spawn_forever` rollup task does when it finishes while
        // Fleet is still mounted.
        dom.in_runtime(|| {
            *FLEET_ROLLUP_FRESH.write() = Some(rollup("2026-09-09"));
        });
        pump(&mut dom);

        let shape: Vec<bool> = SEEN.with(|s| s.borrow().iter().map(|r| r.is_some()).collect());
        let picked = SEEN.with(|s| {
            s.borrow()
                .iter()
                .filter_map(|r| r.as_ref().map(|r| r.date.to_string()))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            picked,
            vec!["2026-09-09".to_string()],
            "a write after mount must re-run the effect (only .read() subscribes); runs were {shape:?}"
        );
        dom.in_runtime(|| {
            assert!(
                FLEET_ROLLUP_FRESH.read().is_none(),
                "the drain must clear the staging slot"
            );
        });
    }

    #[test]
    fn idle_effect_runs_do_not_loop_forever() {
        let mut dom = VirtualDom::new(RollupProbe);
        crate::test_support::rebuild_or_fail(&mut dom);
        pump(&mut dom);
        dom.in_runtime(|| {
            *FLEET_ROLLUP_FRESH.write() = Some(rollup("2026-09-09"));
        });
        pump(&mut dom);

        // Mount run + the drain run. If the drain wrote unconditionally it
        // would re-dirty its own subscription and spin.
        let runs = SEEN.with(|s| s.borrow().len());
        assert!(
            runs <= 3,
            "effect should settle after draining, ran {runs} times"
        );
    }
}
