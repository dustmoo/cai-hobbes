use super::shared::Toggle;
use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::*;

// ── Fleet (Behavior tab card, Pro) ──────────────────────────────────────────

/// The Fleet card: a single Connect/Disconnect action that commits
/// atomically — hook registration in `~/.claude/settings.json` (timestamped
/// backup first), `fleet_enabled`, and an immediate settings save (the
/// calendar-row idiom, not the tab's draft + Save flow). The supervisor loop
/// in main.rs follows `fleet_enabled` and starts/stops the listener within a
/// few seconds; the status line polls so that transition is visible.
#[component]
pub(super) fn FleetSection() -> Element {
    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;
    let fleet_state = use_context::<Signal<crate::fleet::FleetState>>();

    // Bumped by the buttons and a slow poll: the hooks file and the running
    // port live outside the signal graph, so re-probe while the card is up
    // (the listener trails Connect by up to one 5s supervisor tick).
    let mut status_rev = use_signal(|| 0u32);
    let mut fleet_error = use_signal(|| Option::<String>::None);
    let _ = status_rev();
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            status_rev += 1;
        }
    });

    let pro = crate::entitlement::pro_active();
    let hooks_path = crate::fleet::hooks_config::claude_settings_path();
    let connected_port = hooks_path
        .as_ref()
        .and_then(|p| crate::fleet::hooks_config::connected_port_file(p));
    let running_port = crate::fleet::running_port();
    let enabled = settings.read().fleet_enabled;
    let session_count = fleet_state.read().sessions.len();

    let persist = move || {
        settings_manager
            .peek()
            .save_async(settings.peek().clone(), Some(save_error));
    };

    let on_connect = move |_| {
        let Some(path) = crate::fleet::hooks_config::claude_settings_path() else {
            fleet_error.set(Some("Could not resolve your home directory.".into()));
            return;
        };
        // Identity (port + token) persists in meta so re-connects register
        // the same URLs the listener answers on.
        let (port, token) = crate::fleet::server::ensure_identity();
        match crate::fleet::hooks_config::connect_file(&path, port, &token) {
            Ok(()) => {
                fleet_error.set(None);
                settings.write().fleet_enabled = true;
                persist();
            }
            Err(e) => fleet_error.set(Some(e)),
        }
        status_rev += 1;
    };

    let on_disconnect = move |_| {
        let Some(path) = crate::fleet::hooks_config::claude_settings_path() else {
            return;
        };
        match crate::fleet::hooks_config::disconnect_file(&path) {
            Ok(()) => {
                fleet_error.set(None);
                settings.write().fleet_enabled = false;
                persist();
            }
            Err(e) => fleet_error.set(Some(e)),
        }
        status_rev += 1;
    };

    // Tri-state status: dot color + headline + supporting line. Half-states
    // (hooks without listener, port drift) surface as "needs attention" with
    // the one-click fix named — Connect repairs all of them.
    // Port match alone isn't "connected" — the entry set can drift when a
    // build changes the hooks Hobbes registers (the supervisor self-heals
    // this shortly after launch; the card names it in the meantime).
    let hooks_current = match (&hooks_path, connected_port) {
        (Some(path), Some(_)) => {
            let (port, token) = crate::fleet::server::ensure_identity();
            crate::fleet::hooks_config::file_hooks_current(path, port, &token)
        }
        _ => true,
    };
    let (dot_color, headline, detail) = match (connected_port, running_port) {
        (Some(cp), Some(rp)) if cp == rp && !hooks_current => (
            "#f59e0b",
            "Needs attention".to_string(),
            "This build registers a different hook set than the one in ~/.claude/settings.json — hit Connect to update it.".to_string(),
        ),
        (Some(cp), Some(rp)) if cp == rp => (
            "#22c55e",
            "Connected".to_string(),
            format!(
                "Watching {session_count} session{} · port {cp}. Sessions appear in the Fleet tab.",
                if session_count == 1 { "" } else { "s" }
            ),
        ),
        (Some(cp), Some(rp)) => (
            "#f59e0b",
            "Needs attention".to_string(),
            format!("Hooks point at port {cp} but the listener is on {rp} — hit Connect to update them."),
        ),
        (Some(_), None) if enabled => (
            "#f59e0b",
            "Connected".to_string(),
            "Listener starting…".to_string(),
        ),
        (Some(cp), None) => (
            "#f59e0b",
            "Needs attention".to_string(),
            format!("Hooks are registered (port {cp}) but observation is off — hit Connect to restart it."),
        ),
        (None, _) => (
            "#6b7280",
            "Not connected".to_string(),
            "Hobbes isn't watching any sessions.".to_string(),
        ),
    };
    let hooks_registered = connected_port.is_some();

    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-lg",
                h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium mb-1", "Fleet" }
                p {
                    class: "text-xs text-fg-muted mb-3",
                    "Watch every Claude Code session on this machine — status, attention requests, and approval gates — from inside Hobbes."
                }

                if !pro {
                    p {
                        class: "text-sm text-fg-muted",
                        "Fleet observation is a Hobbes Pro feature. Add a license key under About to unlock it."
                    }
                } else {
                    div {
                        class: "flex items-center gap-2 mb-1",
                        span {
                            class: "inline-block w-2 h-2 rounded-full",
                            style: "background-color: {dot_color}",
                        }
                        span { class: "text-sm font-medium", "{headline}" }
                    }
                    p { class: "text-xs text-fg-muted mb-3 ml-4", "{detail}" }

                    div {
                        class: "flex gap-2",
                        if !hooks_registered || dot_color == "#f59e0b" {
                            button {
                                class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                                onclick: on_connect,
                                "Connect Claude Code"
                            }
                        }
                        if hooks_registered {
                            button {
                                class: "border border-faint text-fg rounded px-3 py-1.5 text-sm hover:bg-card",
                                onclick: on_disconnect,
                                "Disconnect"
                            }
                        }
                    }
                    p {
                        class: "text-xs text-fg-muted mt-2",
                        "Connecting starts a localhost-only listener and merges eight hook entries into ~/.claude/settings.json (a timestamped backup is written first). Disconnect removes exactly those entries and stops observation."
                    }
                    if hooks_registered {
                        div {
                            class: "mt-3 flex items-center gap-2.5",
                            Toggle {
                                checked: settings.read().fleet_briefs_enabled,
                                onchange: move |on: bool| {
                                    settings.write().fleet_briefs_enabled = on;
                                    persist();
                                }
                            }
                            span { class: "text-sm text-fg", "Summarize sessions into re-entry briefs (uses your LLM's summary model)" }
                        }
                        p {
                            class: "text-xs text-fg-muted mt-1 ml-[42px]",
                            "Transcript excerpts are sent only to your own configured provider; briefs are stored locally."
                        }
                        div {
                            class: "mt-3 grid grid-cols-[minmax(0,1fr)_220px] gap-4 items-center",
                            div {
                                div { class: "text-sm text-fg", "Retire idle sessions after" }
                                p { class: "text-xs text-fg-muted", "Minutes of silence before a card moves to \"Earlier today\" (any new activity brings it back, time intact). 15–1440." }
                            }
                            div {
                                class: "flex items-center gap-1 shrink-0 justify-self-end",
                                input {
                                    r#type: "number",
                                    min: "15",
                                    max: "1440",
                                    class: "w-20 h-8 bg-app border border-faint rounded px-2.5 text-sm text-fg text-right focus:outline-none focus:ring-2 focus:ring-primary-500",
                                    value: "{settings.read().fleet_retire_minutes}",
                                    onchange: move |e| {
                                        if let Ok(v) = e.value().parse::<u32>() {
                                            settings.write().fleet_retire_minutes = v.clamp(15, 1440);
                                            persist();
                                        }
                                    }
                                }
                                span { class: "text-xs text-fg-muted", "min" }
                            }
                        }
                        div {
                            class: "mt-3 flex items-center gap-2.5",
                            Toggle {
                                checked: settings.read().dispatch_auto_continue,
                                onchange: move |on: bool| {
                                    settings.write().dispatch_auto_continue = on;
                                    persist();
                                }
                            }
                            span { class: "text-sm text-fg", "Continue the chat when a dispatched agent finishes" }
                        }
                        p {
                            class: "text-xs text-fg-muted mt-1 ml-[42px]",
                            "The chat that dispatched a background agent is handed the run's closing brief and picks the thread back up on its own. Off: the run still reports into the Fleet, but nothing is sent back to the conversation."
                        }
                    }
                    if let Some(err) = fleet_error() {
                        p { class: "text-sm text-red-400 mt-2", "{err}" }
                    }
                }
            }
        }
    }
}
