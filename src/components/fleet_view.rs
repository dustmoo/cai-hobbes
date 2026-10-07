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
    let engaged_minutes = fleet::engaged_seconds_on(&state, today, now) / 60;
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
                    // The exocortex multiplier: agent minutes over ENGAGED
                    // minutes (wall-clock with ≥1 agent working) — average
                    // concurrency while driving the fleet. Breaks, planning,
                    // and walks don't decay it; parallel agents raise it.
                    if let Some((mult, engaged)) =
                        fleet::exocortex_multiplier(agent_minutes, engaged_minutes)
                    {
                        div {
                            class: "flex items-baseline gap-1.5",
                            title: format!(
                                "{} of agent work across {} engaged — average concurrency while you're driving the fleet (the ribbon's peak is the high-water mark)",
                                crate::todo::model::format_minutes(agent_minutes),
                                crate::todo::model::format_minutes(engaged),
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
                    } else {
                        // Not enough signal yet (no agent work, or under 5
                        // engaged minutes) — dim the speedometer instead of
                        // removing it, so the dashboard keeps its shape.
                        div {
                            class: "flex items-baseline gap-1.5",
                            title: "Shows once the fleet has agent work and ≥5 engaged minutes today",
                            Icon {
                                width: 14,
                                height: 14,
                                icon: fi_icons::FiZap,
                                class: "self-center opacity-40",
                                style: "color: #a78bfa;",
                            }
                            span { class: "text-lg font-semibold tabular-nums text-fg-muted opacity-60", "—×" }
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

/// The honesty caption under the ribbon: replayed minutes against the banked
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
            if let Some(c) = caption {
                p { class: "mt-1 text-[10px] text-fg-muted/70", "{c}" }
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
    let dismiss_id = session.id.clone();

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
                // Hide, hover-revealed like the tab-close idiom: the escape
                // hatch for a card whose terminal went away without a
                // SessionEnd (the sweeps never retire needs-attention rows).
                // External only — Hobbes tabs leave the board when they close.
                if !is_hobbes {
                    button {
                        class: "order-last shrink-0 opacity-0 group-hover:opacity-100 transition-opacity text-fg-muted hover:text-fg",
                        title: "Hide this session from the fleet (it comes back on its next event)",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            fleet::shared().dismiss_session(&dismiss_id);
                        },
                        Icon { width: 14, height: 14, icon: fi_icons::FiX }
                    }
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
                        // Background phases are silent by design — a bare
                        // dot reads as idle, so this one state gets words.
                        if let Some(label) = background_label(&session.status, session.last_event_at, now) {
                            span {
                                class: "shrink-0 text-[10px] text-fg-muted",
                                style: "color: {chip_color};",
                                { label }
                            }
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
                        // Hidden while a turn or background phase runs: a
                        // session doing work is not blocked-waiting on the
                        // user, whatever an older brief believed.
                        if let Some(blocked) = &brief.blocked_on {
                            if !turn_running {
                                p {
                                    class: "text-xs",
                                    style: "color: #f59e0b;",
                                    { format!("blocked: {}", fleet::truncate_summary(blocked, 100)) }
                                }
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
