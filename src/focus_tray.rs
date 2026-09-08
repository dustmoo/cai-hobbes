//! Focus-session timer on the (single) Hobbes tray icon.
//!
//! There is exactly ONE tray icon. Ownership is consolidated here: the sync
//! effect in `main.rs` calls [`sync_tray`] with both the `show_tray_icon`
//! setting and the current focus snapshot, and the icon exists iff either
//! wants it ([`tray_should_exist`]). While a todo is in focus (the same
//! `TodoStatus::InProgress` condition the FocusBar renders on), the icon
//! carries a live elapsed-vs-estimate readout: on macOS as menu-bar title
//! text next to the icon ("25m/15m"), on Windows in the tooltip (title text
//! isn't supported there). When focus ends the icon reverts to plain — empty
//! title, default tooltip — and disappears entirely if the setting is off.
//!
//! Click routing (in `tray::ensure_tray_listener`) switches on focus mode at
//! event time: with a focus active, a left-click release surfaces the window
//! and reveals the focused todo in the planner (never hides the window);
//! with no focus, the historical any-click visibility toggle applies.
//!
//! There is deliberately no muda menu attached: dioxus-desktop links muda
//! 0.11 for the main application menu and tray-icon links muda 0.15, and
//! *both* register an Objective-C class named `MudaMenuItem` — building a
//! tray menu after the app menu exists panics at runtime on macOS.
//!
//! Everything that can be pure is pure and unit-tested: the existence
//! decision, the snapshot, and the title/tooltip formatting. The tray-icon
//! calls themselves are a thin shell around them.

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use dioxus_signals::{GlobalSignal, Signal};

use crate::todo::model::format_minutes;
use crate::todo::PlannerState;

/// Truncation budget for the todo title in the tooltip.
pub const TITLE_MAX_CHARS: usize = 40;

/// Whether a focus session currently drives the tray icon. Written by
/// [`sync_tray`] on the main thread, read by the tray listener thread at
/// event time to route clicks — an atomic rather than a signal so the
/// listener thread never touches the Dioxus runtime for a read.
static FOCUS_MODE: AtomicBool = AtomicBool::new(false);

pub fn focus_mode_active() -> bool {
    FOCUS_MODE.load(Ordering::Relaxed)
}

/// Monotonic count of focus-mode left-clicks on the tray icon, written by
/// the tray listener thread (`tray::ensure_tray_listener`) and consumed by a
/// `use_effect` in `main.rs` that surfaces the window and planner.
pub static FOCUS_TRAY_CLICKS: GlobalSignal<u64> = Signal::global(|| 0);

/// A todo id the planner should reveal on its next render: select Today and
/// open the todo's detail card. Set by the tray-click handler in `main.rs`,
/// drained by an effect in `PlannerView` (whose selection state is local to
/// that component — this global is the hand-off across the mount boundary).
pub static PLANNER_REVEAL_TODO: GlobalSignal<Option<String>> = Signal::global(|| None);

// ── Pure logic ──────────────────────────────────────────────────────────────

/// The merged existence decision: the icon lives while the setting wants it
/// OR a focus session needs it as the timer's host.
pub fn tray_should_exist(show_tray_icon: bool, focus_active: bool) -> bool {
    show_tray_icon || focus_active
}

/// Everything the timer needs to render, captured from planner state.
/// `None` means "no focus session — the icon is plain".
#[derive(Debug, Clone, PartialEq)]
pub struct FocusTraySnapshot {
    pub todo_id: String,
    pub title: String,
    pub elapsed_minutes: u32,
    pub estimate_minutes: Option<u32>,
    /// Whether the live focus session is agent-driven (started by the
    /// assistant) — mirrored from the open focus-session row's actor.
    pub agent: bool,
}

/// The focus decision: mirrors the FocusBar exactly — a snapshot exists iff
/// one todo is `InProgress`. Elapsed math is `Todo::elapsed_minutes`, the
/// same helper the FocusBar's readout uses.
pub fn focus_snapshot(state: &PlannerState, now: DateTime<Utc>) -> Option<FocusTraySnapshot> {
    state.focused().map(|t| FocusTraySnapshot {
        todo_id: t.id.clone(),
        title: t.title.clone(),
        elapsed_minutes: t.elapsed_minutes(now),
        estimate_minutes: t.estimate_minutes,
        agent: state
            .open_focus_session_for(&t.id)
            .map(|s| s.actor.is_agent())
            .unwrap_or(false),
    })
}

/// Compact duration for menu-bar real estate: `45` → `"45m"`, `60` → `"1h"`,
/// `65` → `"1h05m"`, `120` → `"2h"`. Unlike [`format_minutes`] there is no
/// space, so the menu-bar title stays one unbroken token.
pub fn compact_minutes(minutes: u32) -> String {
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => format!("{}m", m),
        (h, 0) => format!("{}h", h),
        (h, m) => format!("{}h{:02}m", h, m),
    }
}

/// The menu-bar title (macOS): `"25m"`, or `"25m/15m"` when an estimate
/// exists — elapsed first, the contract second.
pub fn timer_title(snapshot: &FocusTraySnapshot) -> String {
    match snapshot.estimate_minutes {
        Some(est) => format!(
            "{}/{}",
            compact_minutes(snapshot.elapsed_minutes),
            compact_minutes(est)
        ),
        None => compact_minutes(snapshot.elapsed_minutes),
    }
}

/// Char-boundary-safe truncation with an ellipsis. `max_chars` counts the
/// ellipsis itself, so the result never exceeds `max_chars` chars.
pub fn truncate_title(title: &str, max_chars: usize) -> String {
    if title.chars().count() <= max_chars {
        return title.to_string();
    }
    let mut out: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The tooltip (and the full human readout): task title plus the FocusBar's
/// exact wording — `"Xm of Ym"` with an estimate, bare elapsed without.
/// Agent-driven sessions carry an "(agent)" marker after the title, matching
/// the FocusBar's tag.
pub fn tooltip_text(snapshot: &FocusTraySnapshot) -> String {
    let mut title = truncate_title(&snapshot.title, TITLE_MAX_CHARS);
    if snapshot.agent {
        title.push_str(" (agent)");
    }
    match snapshot.estimate_minutes {
        Some(est) => format!(
            "{} — {} of {}",
            title,
            format_minutes(snapshot.elapsed_minutes),
            format_minutes(est)
        ),
        None => format!("{} — {}", title, format_minutes(snapshot.elapsed_minutes)),
    }
}

// ── Thin tray shell (untestable by design — keep logic out of here) ─────────

/// Reconcile the single tray icon with the setting and the focus snapshot.
///
/// - Neither wants it → dropped.
/// - Focus active → created if absent (even with `show_tray_icon` off — the
///   icon is the timer's host for the session), timer title + task tooltip.
/// - No focus, setting on → created if absent, reverted to plain (empty
///   title, app-name tooltip).
///
/// Also records focus mode for the listener thread's click routing. Must be
/// called from the main thread (macOS requirement) — in practice from
/// `use_effect` in `app()`, which is.
pub fn sync_tray(
    slot: &mut Option<tray_icon::TrayIcon>,
    show_tray_icon: bool,
    snapshot: Option<&FocusTraySnapshot>,
) {
    FOCUS_MODE.store(snapshot.is_some(), Ordering::Relaxed);

    if !tray_should_exist(show_tray_icon, snapshot.is_some()) {
        if slot.take().is_some() {
            tracing::debug!("Tray icon removed (setting off, nothing in focus).");
        }
        return;
    }
    if slot.is_none() {
        *slot = Some(crate::tray::init_tray());
        tracing::debug!("Tray icon created.");
    }
    let Some(tray) = slot.as_ref() else { return };

    // set_title renders text in the macOS menu bar; it is a no-op on
    // Windows, where the tooltip carries the timer instead. A leading space
    // lets the text breathe against the icon; `None` clears it entirely.
    tray.set_title(snapshot.map(|s| format!(" {}", timer_title(s))));

    let tooltip = snapshot
        .map(tooltip_text)
        .unwrap_or_else(|| crate::settings::get_app_name().to_string());
    if let Err(e) = tray.set_tooltip(Some(tooltip)) {
        tracing::warn!("Failed to set tray tooltip: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{Todo, TodoStatus};

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn focused_todo(title: &str, started: &str, estimate: Option<u32>) -> Todo {
        let mut t = Todo::new(title, 0.0);
        t.status = TodoStatus::InProgress;
        t.started_at = Some(utc(started));
        t.estimate_minutes = estimate;
        t
    }

    #[test]
    fn tray_exists_iff_setting_or_focus_wants_it() {
        // The merged model: the setting keeps the icon around, and a focus
        // session hosts itself on the icon even with the setting off.
        assert!(!tray_should_exist(false, false));
        assert!(tray_should_exist(true, false));
        assert!(tray_should_exist(false, true), "focus hosts the timer");
        assert!(tray_should_exist(true, true));
    }
    #[test]
    fn snapshot_exists_iff_a_todo_is_in_focus() {
        let now = utc("2026-08-24T10:25:00Z");
        let mut state = PlannerState::default();
        assert!(
            focus_snapshot(&state, now).is_none(),
            "empty planner → plain icon"
        );

        state.upsert_todo(Todo::new("idle", 0.0));
        assert!(
            focus_snapshot(&state, now).is_none(),
            "open todos alone must not start a timer"
        );

        let focus = focused_todo("Write the report", "2026-08-24T10:00:00Z", Some(15));
        let id = focus.id.clone();
        state.upsert_todo(focus);
        let snap = focus_snapshot(&state, now).expect("focused todo → timer");
        assert_eq!(snap.todo_id, id);
        assert_eq!(snap.title, "Write the report");
        assert_eq!(snap.elapsed_minutes, 25);
        assert_eq!(snap.estimate_minutes, Some(15));

        // stop_focus ends the session → the icon reverts to plain: no
        // snapshot, and with the setting off the icon has no reason to exist.
        state.stop_focus(now);
        assert!(focus_snapshot(&state, now).is_none());
        assert!(!tray_should_exist(false, focus_snapshot(&state, now).is_some()));
    }

    #[test]
    fn snapshot_elapsed_matches_the_focus_bar_math() {
        // Banked minutes from earlier sessions count on top of the live one,
        // exactly as Todo::elapsed_minutes (the FocusBar's helper) computes.
        let now = utc("2026-08-24T10:10:00Z");
        let mut t = focused_todo("Resume", "2026-08-24T10:00:00Z", None);
        t.actual_minutes = 30;
        let mut state = PlannerState::default();
        state.upsert_todo(t);

        let snap = focus_snapshot(&state, now).unwrap();
        assert_eq!(snap.elapsed_minutes, 40, "30 banked + 10 live");
    }

    #[test]
    fn compact_minutes_stays_one_token() {
        assert_eq!(compact_minutes(0), "0m");
        assert_eq!(compact_minutes(45), "45m");
        assert_eq!(compact_minutes(60), "1h");
        assert_eq!(compact_minutes(65), "1h05m");
        assert_eq!(compact_minutes(120), "2h");
        assert_eq!(compact_minutes(125), "2h05m");
        assert_eq!(compact_minutes(600), "10h");
    }

    fn snap(elapsed: u32, estimate: Option<u32>) -> FocusTraySnapshot {
        FocusTraySnapshot {
            todo_id: "td_1".into(),
            title: "Write the report".into(),
            elapsed_minutes: elapsed,
            estimate_minutes: estimate,
            agent: false,
        }
    }

    #[test]
    fn timer_title_shows_elapsed_then_estimate() {
        assert_eq!(timer_title(&snap(25, None)), "25m");
        assert_eq!(timer_title(&snap(25, Some(15))), "25m/15m");
        assert_eq!(timer_title(&snap(65, Some(120))), "1h05m/2h");
        assert_eq!(timer_title(&snap(0, Some(30))), "0m/30m");
    }

    #[test]
    fn tooltip_uses_the_focus_bars_wording() {
        assert_eq!(
            tooltip_text(&snap(25, Some(15))),
            "Write the report — 25m of 15m"
        );
        assert_eq!(tooltip_text(&snap(95, None)), "Write the report — 1h 35m");
    }

    #[test]
    fn agent_sessions_are_marked_in_snapshot_and_tooltip() {
        use crate::todo::model::FocusActor;

        // The snapshot mirrors the open session row's actor.
        let now = utc("2026-08-24T10:25:00Z");
        let mut state = PlannerState::default();
        let focus = focused_todo("Write the report", "2026-08-24T10:00:00Z", Some(15));
        let id = focus.id.clone();
        state.upsert_todo(focus);
        assert!(
            !focus_snapshot(&state, now).unwrap().agent,
            "no session row (pre-migration live focus) reads as person"
        );

        state.upsert_todo(Todo::new("idle", 0.0));
        state.focus_sessions.push(crate::todo::model::FocusSession::open(
            &id,
            utc("2026-08-24T10:00:00Z"),
            FocusActor::Agent {
                session_id: Some("sess-1".into()),
            },
        ));
        let snap = focus_snapshot(&state, now).unwrap();
        assert!(snap.agent);
        assert_eq!(tooltip_text(&snap), "Write the report (agent) — 25m of 15m");

        // The person path is unchanged.
        let mut person = snap.clone();
        person.agent = false;
        assert_eq!(tooltip_text(&person), "Write the report — 25m of 15m");
    }

    #[test]
    fn truncation_is_char_safe_and_budgeted() {
        assert_eq!(truncate_title("short", 40), "short");
        // Exactly at the budget: untouched.
        let exact: String = "x".repeat(40);
        assert_eq!(truncate_title(&exact, 40), exact);
        // Over budget: 39 chars + ellipsis = 40 chars.
        let long: String = "x".repeat(50);
        let cut = truncate_title(&long, 40);
        assert_eq!(cut.chars().count(), 40);
        assert!(cut.ends_with('…'));
        // Multi-byte chars must not split — chars, not bytes.
        let emoji = "日本語のタイトル".repeat(10);
        let cut = truncate_title(&emoji, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));

        let tip = tooltip_text(&FocusTraySnapshot {
            todo_id: "td".into(),
            title: "A very long task title that certainly exceeds forty characters in total".into(),
            elapsed_minutes: 5,
            estimate_minutes: None,
            agent: false,
        });
        assert!(tip.starts_with("A very long task title that certainly e…"));
    }
}
