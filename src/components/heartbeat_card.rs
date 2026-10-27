//! The Heartbeat card at the top of the Fleet view: on/off, a status line,
//! budget counters, and the trail of recent wakes with a reaction row per
//! event. Pure helpers (`status_of`, counters, `reacted`) carry the logic;
//! the component only draws them.

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

use crate::heartbeat::housekeeping::HousekeepingNote;
use crate::heartbeat::state::{Reaction, ReactionKind, ReactionTarget, WakeRecord};
use crate::heartbeat::{budget, HeartbeatState, HEARTBEAT};
use crate::settings::{Settings, SettingsManager};

/// How many wake records the trail shows.
const TRAIL_ROWS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Off,
    Asleep,
    Waking,
    Deferred(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Off => "Off".to_string(),
            Status::Asleep => "Asleep".to_string(),
            Status::Waking => "Waking".to_string(),
            Status::Deferred(r) => format!("Deferred: {r}"),
        }
    }

    fn color(&self) -> &'static str {
        match self {
            Status::Off => "#94a3b8",
            Status::Asleep => "#60a5fa",
            Status::Waking => "#34d399",
            Status::Deferred(_) => "#f59e0b",
        }
    }
}

/// Off, waking (the Heartbeat session is streaming), deferred (the newest
/// wake record was deferred, so nothing has fired since), else asleep.
pub fn status_of(state: &HeartbeatState, enabled: bool, streaming: bool) -> Status {
    if !enabled {
        return Status::Off;
    }
    if streaming {
        return Status::Waking;
    }
    match state.trail.iter().max_by_key(|w| w.at) {
        Some(w) if w.deferred => Status::Deferred(
            w.defer_reason.clone().unwrap_or_else(|| "over budget".to_string()),
        ),
        _ => Status::Asleep,
    }
}

/// Wakes that actually ran in the last 60 minutes.
pub fn wakes_last_hour(trail: &[WakeRecord], now: DateTime<Utc>) -> usize {
    let start = now - Duration::minutes(60);
    trail.iter().filter(|w| !w.deferred && w.at > start && w.at <= now).count()
}

/// The newest wake that ran.
pub fn last_wake_at(trail: &[WakeRecord]) -> Option<DateTime<Utc>> {
    trail.iter().filter(|w| !w.deferred).map(|w| w.at).max()
}

/// Newest first, capped.
pub fn recent_trail<'a>(trail: impl IntoIterator<Item = &'a WakeRecord>, n: usize) -> Vec<&'a WakeRecord> {
    let mut v: Vec<&WakeRecord> = trail.into_iter().collect();
    v.sort_by(|a, b| b.at.cmp(&a.at));
    v.truncate(n);
    v
}

/// Has this reaction already been recorded for the target? Mute matches any
/// kind on events.
pub fn reacted(reactions: &[Reaction], target: &ReactionTarget, kind: &ReactionKind) -> bool {
    reactions.iter().any(|r| {
        &r.target == target
            && match (&r.reaction, kind) {
                (ReactionKind::MutedKind(_), ReactionKind::MutedKind(_)) => true,
                (a, b) => a == b,
            }
    })
}

/// Add a kind to the muted list once.
pub fn mute_kind(muted: &mut Vec<String>, kind: &str) {
    if !muted.iter().any(|k| k == kind) {
        muted.push(kind.to_string());
    }
}

/// Housekeeping notes worth showing: newest first, within the last 24h,
/// after any "Clear trail", at most `n`.
pub fn recent_notes(
    notes: &[HousekeepingNote],
    cleared_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    n: usize,
) -> Vec<&HousekeepingNote> {
    notes
        .iter()
        .rev()
        .filter(|h| now - h.at <= Duration::hours(24))
        .filter(|h| cleared_at.is_none_or(|c| h.at > c))
        .take(n)
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn age_label(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (now - t).num_minutes();
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if mins < 24 * 60 {
        format!("{}h ago", mins / 60)
    } else {
        format!("{}d ago", mins / (24 * 60))
    }
}

/// Dollars as "$0.40"; clamped so zero (or float dust below it) reads
/// "$0.00", never "$-0.00".
pub fn money(usd: f64) -> String {
    format!("${:.2}", usd.max(0.0) + 0.0)
}

/// The strip's status phrase, next move included: "Asleep · 3 queued · next
/// check-in 10:28 · last wake 12m ago". Off and Deferred stand alone (a
/// disabled Heartbeat has no counters; a deferral's reason is the news).
/// `next_label` is the already-formatted local check-in time.
pub fn strip_phrase(
    status: &Status,
    queued: usize,
    next_label: Option<&str>,
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> String {
    match status {
        Status::Off | Status::Deferred(_) => return status.label(),
        _ => {}
    }
    let mut parts = vec![status.label()];
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    if let Some(n) = next_label {
        parts.push(format!("next check-in {n}"));
    }
    if let Some(t) = last {
        parts.push(format!("last wake {}", age_label(t, now)));
    }
    parts.join(" · ")
}

/// A wake that has finished: it carries a cost or an outcome.
fn finished(w: &WakeRecord) -> bool {
    w.cost_usd.is_some() || w.outcome.is_some() || !w.actions.is_empty()
}

/// One line of the "What it did" list.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultItem {
    /// A wake that did something: its actions, newest first.
    Did { at: DateTime<Utc>, actions: Vec<String>, cost: Option<f64> },
    /// Finished wakes that found nothing to do, rolled into one line.
    Quiet { count: usize, cost: f64 },
    /// Deferred wakes, rolled into one line with the newest reason.
    Deferred { count: usize, reason: String },
}

/// Group wakes (any order) into the results list: wakes with actions newest
/// first, then one rollup for no-action wakes, then one for deferrals.
/// Wakes still running are left out.
pub fn group_results(wakes: &[WakeRecord]) -> Vec<ResultItem> {
    let mut sorted: Vec<&WakeRecord> = wakes.iter().collect();
    sorted.sort_by(|a, b| b.at.cmp(&a.at));
    let mut out = Vec::new();
    let (mut quiet, mut quiet_cost) = (0usize, 0.0f64);
    let mut deferred: Option<(usize, String)> = None;
    for w in sorted {
        if w.deferred {
            let e = deferred.get_or_insert_with(|| {
                (0, w.defer_reason.clone().unwrap_or_else(|| "over budget".to_string()))
            });
            e.0 += 1;
        } else if !w.actions.is_empty() {
            out.push(ResultItem::Did { at: w.at, actions: w.actions.clone(), cost: w.cost_usd });
        } else if finished(w) {
            quiet += 1;
            quiet_cost += w.cost_usd.unwrap_or(0.0);
        }
    }
    if quiet > 0 {
        out.push(ResultItem::Quiet { count: quiet, cost: quiet_cost });
    }
    if let Some((count, reason)) = deferred {
        out.push(ResultItem::Deferred { count, reason });
    }
    out
}

/// "3 check-ins found nothing to do · $0.06".
pub fn quiet_line(count: usize, cost: f64) -> String {
    let noun = if count == 1 { "check-in" } else { "check-ins" };
    format!("{count} {noun} found nothing to do · {}", money(cost))
}

/// "Deferred: hourly cap" or "2 wakes deferred: hourly cap".
pub fn deferred_line(count: usize, reason: &str) -> String {
    if count <= 1 {
        format!("Deferred: {reason}")
    } else {
        format!("{count} wakes deferred: {reason}")
    }
}

/// " · last: Dispatched 'Clyde prep'" when the newest finished wake did
/// something; empty otherwise.
pub fn strip_suffix(trail: &[WakeRecord]) -> String {
    match trail.iter().filter(|w| !w.deferred && finished(w)).max_by_key(|w| w.at) {
        Some(w) if !w.actions.is_empty() => format!(" · last: {}", truncate(&w.actions[0], 48)),
        _ => String::new(),
    }
}

/// One-line result of a wake: the defer reason, or cost + outcome.
fn wake_line(w: &WakeRecord) -> String {
    if w.deferred {
        return format!("deferred: {}", w.defer_reason.as_deref().unwrap_or("over budget"));
    }
    let mut parts = Vec::new();
    if let Some(c) = w.cost_usd {
        parts.push(format!("${c:.2}"));
    }
    if let Some(o) = &w.outcome {
        parts.push(o.clone());
    } else if !w.choice_summary.is_empty() {
        parts.push(w.choice_summary.clone());
    }
    truncate(&parts.join(" · "), 120)
}

#[component]
pub fn HeartbeatCard(now: DateTime<Utc>) -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let planner = use_context::<Signal<crate::todo::PlannerState>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut open = use_signal(|| false);
    let mut confirm_clear = use_signal(|| false);
    let mut details = use_signal(|| false);

    let persist = move || {
        settings_manager
            .peek()
            .save_async(settings.peek().clone(), Some(save_error));
    };

    let (enabled, b) = {
        let s = settings.read();
        (s.heartbeat_enabled, budget::Budget::from_settings(&s))
    };
    let hb = HEARTBEAT.read();
    let session_id = hb.session_id.clone();
    let streaming = match (
        &session_id,
        try_consume_context::<crate::components::stream_manager::StreamManagerContext>(),
    ) {
        (Some(id), Some(sm)) => sm.streaming_sessions.read().contains(id),
        _ => false,
    };
    let status = status_of(&hb, enabled, streaming);
    let wakes = wakes_last_hour(&hb.trail, now);
    let spent = budget::spent_since(&hb.trail, budget::local_day_start(now));
    let queued = hb.queue.len();
    let last = last_wake_at(&hb.trail);
    // Whichever comes first: the queue's check-in or the next day anchor
    // (start of day, meeting prep, over capacity, end of day).
    let next_anchor_at = {
        let s = settings.read();
        (s.planner_enabled && s.heartbeat_source_planner)
            .then(|| {
                let day = crate::heartbeat::anchors::DayView::capture(&planner.read(), &s, now);
                crate::heartbeat::anchors::next_anchor(&hb, &day, now).map(|a| a.due_at.max(now))
            })
            .flatten()
    };
    let next_at = match (crate::heartbeat::wake::next_check_in(&hb, b.wakes_per_hour), next_anchor_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let next_label = next_at.map(|t| t.with_timezone(&chrono::Local).format("%-I:%M").to_string());
    let visible: Vec<WakeRecord> = hb.visible_trail().into_iter().cloned().collect();
    let results = group_results(&recent_trail(visible.iter(), TRAIL_ROWS * 2).into_iter().cloned().collect::<Vec<_>>());
    let strip_extra = strip_suffix(&visible);
    let trail: Vec<WakeRecord> = recent_trail(hb.visible_trail(), TRAIL_ROWS).into_iter().cloned().collect();
    // Pre-resolve each wake's events (summary, kind) and reactions so the
    // rsx holds no borrow of the global.
    struct EvRow {
        id: String,
        kind: String,
        summary: String,
        useful: bool,
        dismissed: bool,
        muted: bool,
    }
    let rows: Vec<(WakeRecord, Vec<EvRow>)> = trail
        .into_iter()
        .map(|w| {
            let evs = w
                .event_ids
                .iter()
                .filter_map(|id| hb.event(id))
                .map(|e| {
                    let t = ReactionTarget::Event(e.id.clone());
                    EvRow {
                        id: e.id.clone(),
                        kind: e.kind.clone(),
                        summary: e.summary.clone(),
                        useful: reacted(&hb.reactions, &t, &ReactionKind::Useful),
                        dismissed: reacted(&hb.reactions, &t, &ReactionKind::Dismissed),
                        muted: reacted(&hb.reactions, &t, &ReactionKind::MutedKind(String::new())),
                    }
                })
                .collect();
            (w, evs)
        })
        .collect();
    let notes: Vec<HousekeepingNote> =
        recent_notes(&hb.housekeeping, hb.trail_cleared_at, now, 5).into_iter().cloned().collect();
    drop(hb);

    let react = move |event_id: String, kind: ReactionKind| {
        HEARTBEAT.write().record_reaction(ReactionTarget::Event(event_id), kind.clone(), Utc::now());
        if let ReactionKind::MutedKind(k) = &kind {
            mute_kind(&mut settings.write().heartbeat_muted_kinds, k);
            persist();
        }
        crate::heartbeat::save_signal();
    };

    let color = status.color();
    // A deferral is the one state that needs eyes: the strip stays open.
    let pinned = matches!(status, Status::Deferred(_));
    let expanded = pinned || *open.read();
    let mut phrase = strip_phrase(&status, queued, next_label.as_deref(), last, now);
    if matches!(status, Status::Asleep | Status::Waking) {
        phrase.push_str(&strip_extra);
    }
    rsx! {
        div {
            class: "mx-6 mb-3 rounded border border-subtle bg-section",
            div {
                class: "min-h-11 px-3 flex items-center gap-2.5",
                button {
                    class: "flex items-center gap-2.5 min-w-0 flex-1 text-left select-none",
                    aria_expanded: "{expanded}",
                    onclick: move |_| {
                        if !pinned {
                            let v = !*open.peek();
                            open.set(v);
                        }
                    },
                    span { class: "text-[11px] text-fg-muted w-3 shrink-0", if expanded { "▾" } else { "▸" } }
                    h2 { class: "text-sm font-semibold shrink-0", "Heartbeat" }
                    span { class: "inline-block w-2 h-2 rounded-full shrink-0", style: "background: {color};" }
                    span {
                        class: if pinned { "text-xs truncate min-w-0" } else { "text-xs truncate min-w-0 text-fg-muted" },
                        style: if pinned { format!("color: {color};") } else { String::new() },
                        "{phrase}"
                    }
                }
                if enabled {
                    span {
                        class: "text-xs text-fg-muted shrink-0 tabular-nums",
                        { format!("{} of {} today", money(spent), money(b.daily_usd)) }
                    }
                }
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox text-primary-600 focus:ring-primary-500 rounded border-faint bg-input",
                    checked: "{enabled}",
                    onchange: move |e| {
                        settings.write().heartbeat_enabled = e.value() == "true";
                        persist();
                    }
                }
            }
            if expanded {
                div {
                    class: "px-3 pb-3",
                    div {
                        class: "flex items-center gap-6 flex-wrap text-xs text-fg-muted",
                        span { { format!("Wakes this hour: {} of {}", wakes, b.wakes_per_hour) } }
                        div { class: "flex-1" }
                        if !rows.is_empty() {
                            if *confirm_clear.read() {
                                span { class: "text-xs text-fg-muted", "Clear the trail? Budget counts stay." }
                                button {
                                    class: "text-xs px-2 py-1 rounded border border-subtle text-fg hover:bg-card",
                                    onclick: move |_| {
                                        crate::heartbeat::HEARTBEAT.write().clear_trail(Utc::now());
                                        crate::heartbeat::save_signal();
                                        confirm_clear.set(false);
                                    },
                                    "Clear"
                                }
                                button {
                                    class: "text-xs px-2 py-1 rounded text-fg-muted hover:text-fg",
                                    onclick: move |_| confirm_clear.set(false),
                                    "Cancel"
                                }
                            } else {
                                button {
                                    class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card",
                                    onclick: move |_| confirm_clear.set(true),
                                    "Clear trail"
                                }
                            }
                        }
                        if let Some(id) = session_id.clone() {
                            button {
                                class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card",
                                onclick: move |_| {
                                    chat_command.set(Some(
                                        crate::components::chat_input::ChatCommand::SwitchToSession(id.clone()),
                                    ));
                                },
                                "Open Heartbeat chat ↗"
                            }
                        }
                    }
            if !notes.is_empty() {
                div {
                    class: "mt-3 space-y-1",
                    h3 { class: "text-xs font-semibold", "Handled" }
                    for note in notes {
                        {
                            let nid = note.id.clone();
                            let can_restore = note.focus_row.is_some() && !note.restored;
                            rsx! {
                                div {
                                    key: "{note.id}",
                                    class: "flex items-baseline gap-2 text-xs",
                                    span { class: "text-fg-muted tabular-nums shrink-0", { age_label(note.at, now) } }
                                    span {
                                        class: "min-w-0 flex-1 truncate",
                                        title: "{note.text}",
                                        { truncate(&note.text, 140) }
                                        if note.restored {
                                            span { class: "text-fg-muted", " · real time kept" }
                                        }
                                    }
                                    if can_restore {
                                        button {
                                            class: "text-xs px-2 py-1 rounded border border-subtle text-fg-muted hover:text-fg hover:bg-card shrink-0",
                                            title: "Bank the full elapsed time instead of the estimate",
                                            onclick: move |_| crate::heartbeat::runtime::restore_runaway(planner, &nid),
                                            "Keep real time"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if rows.is_empty() {
                p {
                    class: "mt-2 text-xs text-fg-muted",
                    "Watches the planner, Fleet and CI, and wakes its own chat when something needs attention."
                }
            } else {
                div {
                    class: "mt-3 space-y-1",
                    h3 { class: "text-xs font-semibold", "What it did" }
                    if results.is_empty() {
                        p { class: "text-xs text-fg-muted", "Nothing yet." }
                    }
                    for item in results {
                        match item {
                            ResultItem::Did { at, actions, cost } => rsx! {
                                div {
                                    class: "flex items-baseline gap-2 text-xs",
                                    span { class: "text-fg-muted tabular-nums shrink-0", { age_label(at, now) } }
                                    div {
                                        class: "min-w-0 flex-1",
                                        for a in actions {
                                            div { class: "truncate", { truncate(&a, 140) } }
                                        }
                                    }
                                    if let Some(c) = cost {
                                        span { class: "text-fg-muted tabular-nums shrink-0", { money(c) } }
                                    }
                                }
                            },
                            ResultItem::Quiet { count, cost } => rsx! {
                                div { class: "text-xs text-fg-muted", { quiet_line(count, cost) } }
                            },
                            ResultItem::Deferred { count, reason } => rsx! {
                                div { class: "text-xs text-fg-muted", { deferred_line(count, &reason) } }
                            },
                        }
                    }
                    button {
                        class: "mt-1 text-xs text-fg-muted hover:text-fg",
                        aria_expanded: "{details}",
                        onclick: move |_| {
                            let v = !*details.peek();
                            details.set(v);
                        },
                        if *details.read() { "Details ▾" } else { "Details ▸" }
                    }
                }
            if *details.read() {
                div {
                    class: "mt-2 space-y-2",
                    for (w, evs) in rows {
                        div {
                            key: "{w.id}",
                            class: "rounded border border-subtle p-2",
                            div {
                                class: "flex items-baseline gap-2 text-xs",
                                span { class: "text-fg-muted tabular-nums shrink-0", { age_label(w.at, now) } }
                                span { class: "font-medium shrink-0", "{w.trigger}" }
                                span { class: "text-fg-muted truncate min-w-0", { wake_line(&w) } }
                            }
                            for e in evs {
                                {
                                    let (id_u, id_d, id_m) = (e.id.clone(), e.id.clone(), e.id.clone());
                                    let (kind_m, kind_label) = (e.kind.clone(), e.kind.clone());
                                    let mut react_u = react;
                                    let mut react_d = react;
                                    let mut react_m = react;
                                    rsx! {
                                        div {
                                            key: "{e.id}",
                                            class: "mt-1 flex items-center gap-2 text-xs flex-wrap",
                                            span { class: "min-w-0 flex-1 truncate", { truncate(&e.summary, 140) } }
                                            button {
                                                class: if e.useful { "px-1.5 py-0.5 rounded text-green-500" } else { "px-1.5 py-0.5 rounded text-fg-muted hover:text-fg hover:bg-card" },
                                                onclick: move |_| react_u(id_u.clone(), ReactionKind::Useful),
                                                "Useful"
                                            }
                                            button {
                                                class: if e.dismissed { "px-1.5 py-0.5 rounded text-green-500" } else { "px-1.5 py-0.5 rounded text-fg-muted hover:text-fg hover:bg-card" },
                                                onclick: move |_| react_d(id_d.clone(), ReactionKind::Dismissed),
                                                "Dismiss"
                                            }
                                            button {
                                                class: if e.muted { "px-1.5 py-0.5 rounded text-green-500" } else { "px-1.5 py-0.5 rounded text-fg-muted hover:text-fg hover:bg-card" },
                                                onclick: move |_| react_m(id_m.clone(), ReactionKind::MutedKind(kind_m.clone())),
                                                { format!("Mute \"{kind_label}\"") }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(m: i64) -> DateTime<Utc> {
        Utc::now() - Duration::minutes(m)
    }

    fn fired(at: DateTime<Utc>) -> WakeRecord {
        WakeRecord::fired(at, "t", vec![], "c")
    }

    fn deferred(at: DateTime<Utc>) -> WakeRecord {
        WakeRecord::deferred(at, "t", vec![], "hourly cap")
    }

    #[test]
    fn recent_notes_newest_first_windowed_and_cleared() {
        let now = Utc::now();
        let note = |id: &str, mins: i64| HousekeepingNote {
            id: id.into(),
            at: now - Duration::minutes(mins),
            text: id.into(),
            focus_row: None,
            restored: false,
            briefed: false,
        };
        let notes = vec![note("old", 25 * 60), note("a", 120), note("b", 60), note("c", 5)];
        let ids = |v: Vec<&HousekeepingNote>| v.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(recent_notes(&notes, None, now, 5)), ["c", "b", "a"]);
        assert_eq!(ids(recent_notes(&notes, None, now, 2)), ["c", "b"]);
        let cleared = Some(now - Duration::minutes(90));
        assert_eq!(ids(recent_notes(&notes, cleared, now, 5)), ["c", "b"]);
    }

    #[test]
    fn status_precedence() {
        let mut s = HeartbeatState::default();
        assert_eq!(status_of(&s, false, true), Status::Off);
        assert_eq!(status_of(&s, true, false), Status::Asleep);
        assert_eq!(status_of(&s, true, true), Status::Waking);
        s.trail.push(fired(t(30)));
        s.trail.push(deferred(t(5)));
        assert_eq!(status_of(&s, true, false), Status::Deferred("hourly cap".into()));
        s.trail.push(fired(t(1)));
        assert_eq!(status_of(&s, true, false), Status::Asleep);
    }

    #[test]
    fn strip_phrase_carries_the_next_move() {
        let now = Utc::now();
        assert_eq!(
            strip_phrase(&Status::Asleep, 3, Some("10:28"), Some(now - Duration::minutes(12)), now),
            "Asleep · 3 queued · next check-in 10:28 · last wake 12m ago"
        );
        // Nothing queued, never woke: just the state.
        assert_eq!(strip_phrase(&Status::Asleep, 0, None, None, now), "Asleep");
        // Off has no counters; Deferred leads with its reason.
        assert_eq!(strip_phrase(&Status::Off, 3, Some("10:28"), Some(now), now), "Off");
        assert_eq!(
            strip_phrase(&Status::Deferred("hourly cap".into()), 3, Some("10:28"), None, now),
            "Deferred: hourly cap"
        );
    }

    #[test]
    fn money_never_shows_negative_zero() {
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(-0.0), "$0.00");
        assert_eq!(money(-0.0004), "$0.00");
        assert_eq!(money(0.4), "$0.40");
        assert_eq!(money(5.0), "$5.00");
    }

    #[test]
    fn counters() {
        let trail = vec![fired(t(90)), fired(t(20)), deferred(t(10))];
        assert_eq!(wakes_last_hour(&trail, Utc::now()), 1);
        assert!(last_wake_at(&trail).unwrap() > t(21));
        let r = recent_trail(&trail, 2);
        assert_eq!(r.len(), 2);
        assert!(r[0].at > r[1].at);
    }

    #[test]
    fn mute_dedupes_and_truncate_is_char_safe() {
        let mut m = vec![];
        mute_kind(&mut m, "ci_failed");
        mute_kind(&mut m, "ci_failed");
        assert_eq!(m.len(), 1);
        assert_eq!(truncate("héllo wörld", 5), "héll…");
        assert_eq!(truncate("ok", 5), "ok");
    }

    #[test]
    fn results_group_actions_quiet_and_deferred() {
        let mut a = fired(t(30));
        a.actions = vec!["Dispatched 'Clyde prep'".into()];
        a.cost_usd = Some(0.05);
        let mut q1 = fired(t(20));
        q1.cost_usd = Some(0.02);
        let mut q2 = fired(t(10));
        q2.outcome = Some("nothing".into());
        q2.cost_usd = Some(0.04);
        let running = fired(t(1));
        let items = group_results(&[q1, deferred(t(5)), a, running, q2, deferred(t(40))]);
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], ResultItem::Did { actions, .. } if actions.len() == 1));
        match &items[1] {
            ResultItem::Quiet { count, cost } => {
                assert_eq!(*count, 2);
                assert_eq!(money(*cost), "$0.06");
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(items[2], ResultItem::Deferred { count: 2, reason: "hourly cap".into() });
    }

    #[test]
    fn quiet_and_deferred_lines() {
        assert_eq!(quiet_line(3, 0.06), "3 check-ins found nothing to do · $0.06");
        assert_eq!(quiet_line(1, 0.0), "1 check-in found nothing to do · $0.00");
        assert_eq!(deferred_line(1, "cap"), "Deferred: cap");
        assert_eq!(deferred_line(2, "cap"), "2 wakes deferred: cap");
    }

    #[test]
    fn strip_suffix_uses_latest_finished_wake() {
        assert_eq!(strip_suffix(&[]), "");
        let mut old = fired(t(30));
        old.actions = vec!["Old thing".into()];
        let mut quiet = fired(t(5));
        quiet.cost_usd = Some(0.01);
        // Newest finished wake did nothing: no suffix.
        assert_eq!(strip_suffix(&[old.clone(), quiet]), "");
        let mut new = fired(t(2));
        new.actions = vec!["Dispatched 'Clyde prep'".into(), "second".into()];
        assert_eq!(strip_suffix(&[old, new.clone(), deferred(t(1))]), " · last: Dispatched 'Clyde prep'");
        new.actions = vec!["x".repeat(80)];
        assert!(strip_suffix(&[new]).chars().count() <= " · last: ".chars().count() + 48);
    }
}
