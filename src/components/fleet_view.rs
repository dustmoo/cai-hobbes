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
    let enabled = settings.read().fleet_enabled;
    let connected = fleet::hooks_config::claude_settings_path()
        .and_then(|p| fleet::hooks_config::connected_port_file(&p))
        .is_some();
    drop(state);

    rsx! {
        div {
            class: "flex-1 min-w-0 flex flex-col overflow-y-auto",
            div {
                class: "px-6 pt-6 pb-2 flex items-baseline gap-3",
                h1 { class: "text-xl font-semibold", "Fleet" }
                if let Some(port) = fleet::running_port() {
                    span { class: "text-sm text-fg-muted", "listening on 127.0.0.1:{port}" }
                } else {
                    span { class: "text-sm text-fg-muted", "listener stopped" }
                }
            }

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

            div {
                class: "px-6 space-y-2 pb-16",
                for session in sessions {
                    FleetRow { key: "{session.id}", session: session.clone(), now, today }
                }
            }
        }
    }
}

#[component]
fn FleetRow(session: FleetSession, now: DateTime<Utc>, today: chrono::NaiveDate) -> Element {
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
    let session_id = session.id.clone();
    let toggle_id = session.id.clone();

    rsx! {
        div {
            class: "rounded border border-subtle bg-section p-3",
            style: "{row_style}",
            div {
                class: "flex items-center gap-3",
                Icon { width: 16, height: 16, icon: fi_icons::FiTerminal }
                div {
                    class: "flex-1 min-w-0",
                    div {
                        class: "flex items-baseline gap-2",
                        span { class: "text-sm font-semibold truncate", "{session.name}" }
                        span {
                            class: "text-xs px-1.5 py-0.5 rounded-full font-medium",
                            style: "color: {chip_color}; border: 1px solid {chip_color};",
                            "{chip_label}"
                        }
                    }
                    p { class: "text-xs text-fg-muted truncate", "{session.cwd}" }
                }
                div {
                    class: "text-right shrink-0",
                    p { class: "text-xs text-fg-muted", { age_label(session.last_event_at, now) } }
                    if minutes_today > 0 {
                        p { class: "text-xs text-fg-muted", { format!("{} today", format_minutes(minutes_today)) } }
                    }
                }
            }

            // Attention detail line (what the session wants).
            if let FleetStatus::NeedsAttention(AttentionKind::Notification { message, .. }) = &session.status {
                if !message.is_empty() {
                    p { class: "mt-2 text-xs text-fg", "{message}" }
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
            // straight to the terminal prompt.
            label {
                class: "mt-2 flex items-center gap-2 text-xs text-fg-muted cursor-pointer select-none",
                input {
                    r#type: "checkbox",
                    checked: auto,
                    onchange: move |e| {
                        let _ = &session_id;
                        fleet::shared().set_auto_passthrough(&toggle_id, e.value() == "true");
                    },
                }
                "Answer permission prompts in the terminal (skip in-app approval)"
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
