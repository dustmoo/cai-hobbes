//! The Heartbeat card at the top of the Fleet view: on/off, a status line,
//! budget counters, and the trail of recent wakes with a reaction row per
//! event. Pure helpers (`status_of`, counters, `reacted`) carry the logic;
//! the component only draws them.

use chrono::{DateTime, Duration, Utc};
use dioxus::prelude::*;

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
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;
    let mut chat_command =
        use_context::<Signal<Option<crate::components::chat_input::ChatCommand>>>();
    let mut open = use_signal(|| false);
    let mut confirm_clear = use_signal(|| false);

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
    let next_label = crate::heartbeat::wake::next_check_in(&hb, b.wakes_per_hour)
        .map(|t| t.with_timezone(&chrono::Local).format("%-I:%M").to_string());
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
    let phrase = strip_phrase(&status, queued, next_label.as_deref(), last, now);
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
            if rows.is_empty() {
                p {
                    class: "mt-2 text-xs text-fg-muted",
                    "Watches the planner, Fleet and CI, and wakes its own chat when something needs attention."
                }
            } else {
                div {
                    class: "mt-3 space-y-2",
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
}
