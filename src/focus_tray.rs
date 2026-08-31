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

// ── Local vs Cloud privacy indicator ────────────────────────────────────────

/// Whether AI data leaves this machine — the at-a-glance risk signal shown
/// as a leading glyph in the tray title (macOS) or tooltip prefix (Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataLocality {
    Local,
    Cloud,
}

/// Whether an OpenAI-compatible endpoint points at THIS machine: loopback
/// (127.0.0.0/8, ::1), the unspecified addresses (0.0.0.0, ::), `localhost`,
/// or an mDNS `*.local` name. Anything unparseable or empty is NOT local —
/// the indicator must fail toward "cloud".
pub fn is_local_endpoint(endpoint: &str) -> bool {
    let e = endpoint.trim();
    if e.is_empty() {
        return false;
    }
    // Hand-rolled authority extraction (scheme, userinfo, port, path) — the
    // endpoint is a user-typed base URL, not always a well-formed URL.
    let rest = e.split_once("://").map(|(_, r)| r).unwrap_or(e);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    // IP literals: loopback or unspecified count as this machine. Parsing
    // also protects against lookalikes — "127.evil.com" is not an IP.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback() || ip.is_unspecified();
    }
    host == "localhost" || host.ends_with(".local")
}

/// The resolved privacy status: locality plus what the tooltip should name.
#[derive(Debug, Clone, PartialEq)]
pub struct PrivacyStatus {
    pub locality: DataLocality,
    /// Display name of the connector serving the session.
    pub provider_label: Option<String>,
    /// Whether Composio (cloud tool execution) is configured.
    pub composio: bool,
}

/// Map the session's effective connector + Composio configuration to a
/// locality. CLOUD if the connector is Gemini or Claude, if an
/// OpenAI-compatible endpoint points off-machine, or if any Composio profile
/// exists (tool calls execute through Composio's cloud regardless of the
/// model host). LOCAL only for a loopback/local OpenAI-compatible endpoint
/// with no Composio. No connector at all is CLOUD — for a risk indicator,
/// "unknown" must read as the risky direction, never the reassuring one.
pub fn privacy_status(
    connector: Option<&crate::settings::ProviderInstance>,
    has_composio: bool,
) -> PrivacyStatus {
    use crate::settings::ProviderInstanceConfig as C;
    let connector_local = match connector.map(|c| &c.config) {
        Some(C::OpenAiCompat(c)) => is_local_endpoint(&c.endpoint),
        Some(C::Gemini(_)) | Some(C::Claude(_)) | None => false,
    };
    let locality = if connector_local && !has_composio {
        DataLocality::Local
    } else {
        DataLocality::Cloud
    };
    let provider_label = connector.map(|c| {
        let name = c.name.trim();
        if name.is_empty() {
            c.provider().display_name().to_string()
        } else {
            name.to_string()
        }
    });
    PrivacyStatus {
        locality,
        provider_label,
        composio: has_composio,
    }
}

impl PrivacyStatus {
    pub fn glyph(&self) -> &'static str {
        match self.locality {
            DataLocality::Local => "⌂",
            DataLocality::Cloud => "☁",
        }
    }

    /// Plain-language explanation for the tooltip.
    pub fn line(&self) -> String {
        match self.locality {
            DataLocality::Local => "Local: AI processing stays on this machine".to_string(),
            DataLocality::Cloud => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(p) = &self.provider_label {
                    parts.push(p.clone());
                }
                if self.composio {
                    parts.push("Composio".to_string());
                }
                if parts.is_empty() {
                    "Cloud: prompts and tool calls leave this machine".to_string()
                } else {
                    format!(
                        "Cloud: prompts and tool calls leave this machine ({})",
                        parts.join(", ")
                    )
                }
            }
        }
    }
}

/// The full tray title: a leading space so the text breathes against the
/// icon, then the indicator glyph, then the timer, two spaces apart —
/// indicator first, timer second. `None` when there is nothing to show
/// (`set_title(None)` clears the text entirely, so an indicator-only or
/// empty title never leaves a trailing-space artifact).
pub fn compose_title(indicator: Option<&str>, timer: Option<&str>) -> Option<String> {
    let body = match (indicator, timer) {
        (Some(g), Some(t)) => format!("{}  {}", g, t),
        (Some(g), None) => g.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => return None,
    };
    Some(format!(" {}", body))
}

/// The full tooltip: the base readout (task line while focused, the app
/// name otherwise), plus — when the indicator is on — the privacy line, and
/// the glyph as a prefix where the platform has no title text (Windows).
pub fn compose_tooltip(base: &str, privacy: Option<&PrivacyStatus>, glyph_prefix: bool) -> String {
    let Some(p) = privacy else {
        return base.to_string();
    };
    let mut out = if glyph_prefix {
        format!("{} {}", p.glyph(), base)
    } else {
        base.to_string()
    };
    out.push('\n');
    out.push_str(&p.line());
    out
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
/// The privacy indicator (when enabled) rides along whenever the icon
/// exists — it never summons the icon on its own (`privacy` plays no part
/// in [`tray_should_exist`]).
///
/// Also records focus mode for the listener thread's click routing. Must be
/// called from the main thread (macOS requirement) — in practice from
/// `use_effect` in `app()`, which is.
pub fn sync_tray(
    slot: &mut Option<tray_icon::TrayIcon>,
    show_tray_icon: bool,
    snapshot: Option<&FocusTraySnapshot>,
    privacy: Option<&PrivacyStatus>,
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
    // Windows, where the tooltip carries the timer and the glyph instead.
    let timer = snapshot.map(timer_title);
    tray.set_title(compose_title(privacy.map(|p| p.glyph()), timer.as_deref()));

    let base = snapshot
        .map(tooltip_text)
        .unwrap_or_else(|| crate::settings::get_app_name().to_string());
    let tooltip = compose_tooltip(&base, privacy, cfg!(not(target_os = "macos")));
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
        // session hosts itself on the icon even with the setting off. The
        // privacy indicator is deliberately NOT an input — it rides along
        // when the icon exists but never summons it.
        assert!(!tray_should_exist(false, false));
        assert!(tray_should_exist(true, false));
        assert!(tray_should_exist(false, true), "focus hosts the timer");
        assert!(tray_should_exist(true, true));
    }

    fn connector(
        config: crate::settings::ProviderInstanceConfig,
        name: &str,
    ) -> crate::settings::ProviderInstance {
        crate::settings::ProviderInstance {
            id: "c1".into(),
            name: name.into(),
            config,
        }
    }

    fn openai(endpoint: &str) -> crate::settings::ProviderInstance {
        connector(
            crate::settings::ProviderInstanceConfig::OpenAiCompat(
                crate::llm::config::OpenAiCompatConfig {
                    endpoint: endpoint.into(),
                    ..Default::default()
                },
            ),
            "Ollama",
        )
    }

    #[test]
    fn local_endpoint_detection_covers_loopback_variants() {
        for local in [
            "http://localhost:11434/v1",
            "HTTP://LOCALHOST:11434",
            "http://127.0.0.1:8080/v1",
            "http://127.1.2.3:8080", // all of 127.0.0.0/8 is loopback
            "127.0.0.1:8080",        // scheme-less user input
            "http://[::1]:8000/v1",
            "http://0.0.0.0:8000",
            "http://mymac.local:1234/api",
            "http://user:pass@localhost:9/v1",
        ] {
            assert!(is_local_endpoint(local), "{local} should be local");
        }
        for remote in [
            "",
            "   ",
            "https://api.openai.com/v1",
            "https://127.evil.com/v1",      // IP-lookalike hostname
            "http://localhost.evil.com/v1", // localhost-lookalike hostname
            "https://mylocal.example.com",  // ".local" must be a suffix label
            "https://192.168.1.20:11434",   // LAN is still off this machine
        ] {
            assert!(!is_local_endpoint(remote), "{remote} should NOT be local");
        }
    }

    #[test]
    fn risk_mapping_covers_every_branch() {
        use crate::llm::config::{ClaudeConfig, GeminiConfig};
        use crate::settings::ProviderInstanceConfig as C;

        // Cloud providers are cloud, regardless of anything else.
        let gemini = connector(C::Gemini(GeminiConfig::default()), "Gemini");
        assert_eq!(privacy_status(Some(&gemini), false).locality, DataLocality::Cloud);
        let claude = connector(C::Claude(ClaudeConfig::default()), "Claude");
        assert_eq!(privacy_status(Some(&claude), false).locality, DataLocality::Cloud);

        // OpenAI-compat: local iff the endpoint stays on this machine.
        let ollama = openai("http://localhost:11434/v1");
        assert_eq!(privacy_status(Some(&ollama), false).locality, DataLocality::Local);
        let remote = openai("https://api.together.xyz/v1");
        assert_eq!(privacy_status(Some(&remote), false).locality, DataLocality::Cloud);
        // An unconfigured (empty) endpoint is unknown → cloud, the safe read.
        let blank = openai("");
        assert_eq!(privacy_status(Some(&blank), false).locality, DataLocality::Cloud);

        // Composio is cloud tool execution: it overrides a local connector.
        let status = privacy_status(Some(&ollama), true);
        assert_eq!(status.locality, DataLocality::Cloud);
        assert!(status.composio);

        // No connector at all → cloud. A risk indicator fails toward risk.
        assert_eq!(privacy_status(None, false).locality, DataLocality::Cloud);

        // The tooltip label prefers the instance name, falls back to the kind.
        assert_eq!(status.provider_label.as_deref(), Some("Ollama"));
        let unnamed = connector(C::Gemini(GeminiConfig::default()), "  ");
        assert_eq!(
            privacy_status(Some(&unnamed), false).provider_label.as_deref(),
            Some("Gemini")
        );
    }

    #[test]
    fn title_composes_indicator_then_timer_with_breathing_room() {
        // Leading space separates the text from the icon; two spaces sit
        // between the glyph and the timer.
        assert_eq!(
            compose_title(Some("☁"), Some("3m/1h")).as_deref(),
            Some(" ☁  3m/1h")
        );
        // Indicator alone: no trailing-space artifact.
        let alone = compose_title(Some("⌂"), None).unwrap();
        assert_eq!(alone, " ⌂");
        assert_eq!(alone.trim_end(), alone);
        // Timer alone (indicator setting off): unchanged behaviour.
        assert_eq!(compose_title(None, Some("25m/15m")).as_deref(), Some(" 25m/15m"));
        // Neither → None, which clears set_title entirely.
        assert_eq!(compose_title(None, None), None);
    }

    #[test]
    fn tooltip_explains_the_indicator_in_plain_language() {
        let local = privacy_status(Some(&openai("http://localhost:11434/v1")), false);
        assert_eq!(
            compose_tooltip("Hobbes", Some(&local), false),
            "Hobbes\nLocal: AI processing stays on this machine"
        );

        let cloud = privacy_status(
            Some(&connector(
                crate::settings::ProviderInstanceConfig::Gemini(
                    crate::llm::config::GeminiConfig::default(),
                ),
                "Gemini",
            )),
            true,
        );
        assert_eq!(
            compose_tooltip("Write the report — 25m of 15m", Some(&cloud), false),
            "Write the report — 25m of 15m\nCloud: prompts and tool calls leave this machine (Gemini, Composio)"
        );
        // No connector: the cloud line still reads cleanly, without parens.
        let unknown = privacy_status(None, false);
        assert_eq!(
            compose_tooltip("Hobbes", Some(&unknown), false),
            "Hobbes\nCloud: prompts and tool calls leave this machine"
        );

        // Windows carries the glyph as a tooltip prefix (no title text there).
        assert_eq!(
            compose_tooltip("Hobbes", Some(&local), true),
            "⌂ Hobbes\nLocal: AI processing stays on this machine"
        );
        // Indicator off → the base tooltip is untouched.
        assert_eq!(compose_tooltip("Hobbes", None, true), "Hobbes");
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
