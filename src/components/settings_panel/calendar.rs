use crate::SecretManagerTrait;
use crate::settings::{Settings, SettingsManager};
use dioxus::prelude::*;
use dioxus_free_icons::{Icon, icons::fi_icons};

// ── Calendar subscriptions (Planner tab) ────────────────────────────────────

/// Preset swatches for calendar subscriptions — a small fixed palette, not a
/// free color picker. New subscriptions cycle through it by index.
pub(crate) const CALENDAR_COLOR_PALETTE: [&str; 7] = [
    "#4f9cf9", // blue
    "#34d399", // emerald
    "#f59e0b", // amber
    "#f472b6", // pink
    "#a78bfa", // violet
    "#f87171", // red
    "#22d3ee", // cyan
];

/// Auto-pick a color for the `index`-th subscription, cycling the palette.
pub(crate) fn pick_default_color(index: usize) -> &'static str {
    CALENDAR_COLOR_PALETTE[index % CALENDAR_COLOR_PALETTE.len()]
}

/// Validate a pasted ICS feed URL: non-empty, `http(s)://` or `webcal://`
/// scheme (case-insensitive), and a non-empty host. The sync loop rewrites
/// `webcal://` to `https://` at fetch time.
fn validate_ics_url(raw: &str) -> Result<(), String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("Enter the calendar's feed URL.".to_string());
    }
    let lower = url.to_ascii_lowercase();
    let rest = ["https://", "http://", "webcal://"]
        .iter()
        .find_map(|scheme| lower.strip_prefix(scheme));
    match rest {
        None => Err("The URL must start with https://, http://, or webcal://.".to_string()),
        Some(rest) if rest.split(['/', '?', '#']).next().unwrap_or("").is_empty() => {
            Err("The URL needs a host after the scheme.".to_string())
        }
        Some(_) => Ok(()),
    }
}

/// Friendly relative time for the sync status line: "just now", "5m ago",
/// "3h ago", then the planner's friendly date for anything older.
fn friendly_sync_time(
    t: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = (now - t).num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    crate::todo::views::friendly_date(
        t.with_timezone(&chrono::Local).date_naive(),
        now.with_timezone(&chrono::Local).date_naive(),
    )
}

/// One subscription row: color swatch (palette popover), inline rename,
/// source kind, enabled toggle, sync status, and a two-click armed remove
/// (the planner's confirm-on-delete idiom — no modal).
#[component]
fn CalendarSubscriptionRow(sub: crate::settings::CalendarSubscription) -> Element {
    use crate::settings::CalendarSource;
    use crate::todo::calendar_sync::{self, CalendarSyncMsg};

    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();
    let mut planner = use_context::<Signal<crate::todo::PlannerState>>();
    let sync = use_context::<Coroutine<CalendarSyncMsg>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    let mut palette_open = use_signal(|| false);
    let mut remove_armed = use_signal(|| false);

    // Subscribing to the planner signal refreshes the status line after each
    // sync pass (the sync loop writes the planner signal when it
    // re-materializes) — the meta table itself is not reactive.
    let _ = planner.read().blocks.len();
    let sync_state = calendar_sync::load_sync_state(&sub.id);
    let status = match sync_state.last_synced_at {
        Some(t) => format!("Synced {}", friendly_sync_time(t, chrono::Utc::now())),
        None => "Never synced".to_string(),
    };
    let last_error = sync_state.last_error.clone();

    let kind_label = match &sub.source {
        CalendarSource::Ics {} => "ICS feed".to_string(),
        CalendarSource::Composio { calendar_id, .. } => {
            format!("Google Calendar · {}", calendar_id)
        }
    };

    let persist = move || {
        settings_manager
            .peek()
            .save_async(settings.peek().clone(), Some(save_error));
    };

    let id = sub.id.clone();
    let rename_id = id.clone();
    let toggle_id = id.clone();
    let remove_id = id.clone();
    let is_ics = matches!(sub.source, CalendarSource::Ics {});
    let enabled = sub.enabled;

    rsx! {
        div {
            class: "rounded border border-subtle bg-input/30 px-3 py-2",
            // A wandering pointer disarms the remove confirmation, mirroring
            // the planner's row-delete idiom.
            onmouseleave: move |_| {
                if *remove_armed.peek() {
                    remove_armed.set(false);
                }
            },
            div {
                class: "flex items-center gap-2",
                button {
                    class: "h-4 w-4 shrink-0 rounded-full border border-subtle",
                    style: "background-color: {sub.color};",
                    title: "Change color",
                    onclick: move |_| {
                        let open = *palette_open.peek();
                        palette_open.set(!open);
                    },
                }
                input {
                    r#type: "text",
                    class: "min-w-0 flex-1 rounded border border-transparent bg-transparent px-1 py-0.5 text-sm text-fg hover:border-subtle focus:border-subtle focus:bg-input",
                    value: "{sub.name}",
                    onchange: move |e| {
                        settings.write().rename_calendar_subscription(&rename_id, &e.value());
                        persist();
                    }
                }
                span { class: "shrink-0 text-xs text-fg-muted", "{kind_label}" }
                input {
                    r#type: "checkbox",
                    class: "toggle-checkbox shrink-0 rounded border-faint bg-input text-primary-600 focus:ring-primary-500",
                    title: if enabled { "Enabled — syncing" } else { "Disabled — its events are removed from the timeline" },
                    checked: "{enabled}",
                    onchange: move |e| {
                        if let Some(s) = settings.write().calendar_subscription_by_id_mut(&toggle_id) {
                            s.enabled = e.value() == "true";
                        }
                        persist();
                        // An immediate pass so disabling reaps blocks promptly
                        // (and enabling populates without waiting for a tick).
                        sync.send(CalendarSyncMsg::SyncNow);
                    }
                }
                button {
                    class: if remove_armed() {
                        "shrink-0 rounded bg-red-600 px-2 py-0.5 text-xs font-semibold text-fg hover:bg-red-700"
                    } else {
                        "shrink-0 text-fg-muted hover:text-red-400"
                    },
                    title: if remove_armed() { "Click again to remove" } else { "Remove subscription" },
                    onclick: move |_| {
                        // First click arms; the second (while still on the row)
                        // removes the subscription and all of its traces.
                        if !*remove_armed.peek() {
                            remove_armed.set(true);
                            return;
                        }
                        remove_armed.set(false);
                        settings.write().remove_calendar_subscription(&remove_id);
                        // The keychain feed URL (ICS only), the cached events,
                        // and the materialized blocks all belong to this
                        // subscription — remove every trace.
                        if is_ics {
                            if let Err(e) = secret_manager.write().delete_cal_url(&remove_id) {
                                tracing::warn!("Failed to delete calendar URL from keychain: {}", e);
                            }
                        }
                        if let Err(e) =
                            crate::todo::store::delete_calendar_events_for_subscription(&remove_id)
                        {
                            tracing::error!("Failed to delete cached calendar events: {}", e);
                        }
                        let removed_blocks: Vec<String> = {
                            let mut state = planner.write();
                            let mut removed = Vec::new();
                            state.blocks.retain(|b| match &b.source {
                                crate::todo::model::BlockSource::External {
                                    subscription_id: Some(sid),
                                    ..
                                } if *sid == remove_id => {
                                    removed.push(b.id.clone());
                                    false
                                }
                                _ => true,
                            });
                            removed
                        };
                        for block_id in &removed_blocks {
                            if let Err(e) = crate::todo::store::delete_block(block_id) {
                                tracing::error!("Failed to delete calendar block {}: {}", block_id, e);
                            }
                        }
                        persist();
                    },
                    if remove_armed() {
                        "Remove?"
                    } else {
                        Icon { width: 14, height: 14, icon: fi_icons::FiTrash2 }
                    }
                }
            }
            if palette_open() {
                div {
                    class: "mt-2 flex items-center gap-1.5",
                    for color in CALENDAR_COLOR_PALETTE {
                        {
                            let swatch_id = id.clone();
                            let is_current = sub.color.eq_ignore_ascii_case(color);
                            rsx! {
                                button {
                                    key: "{color}",
                                    class: if is_current {
                                        "h-5 w-5 rounded-full ring-2 ring-fg ring-offset-1 ring-offset-section"
                                    } else {
                                        "h-5 w-5 rounded-full border border-subtle hover:scale-110"
                                    },
                                    style: "background-color: {color};",
                                    onclick: move |_| {
                                        if let Some(s) =
                                            settings.write().calendar_subscription_by_id_mut(&swatch_id)
                                        {
                                            s.color = color.to_string();
                                        }
                                        palette_open.set(false);
                                        persist();
                                    },
                                }
                            }
                        }
                    }
                }
            }
            div {
                class: "mt-1 flex items-center gap-2 text-xs text-fg-muted",
                span { "{status}" }
                if let Some(err) = last_error {
                    span {
                        class: "min-w-0 truncate text-red-400",
                        title: "{err}",
                        "{err}"
                    }
                }
            }
        }
    }
}

/// The "Calendar subscriptions" section of the Planner settings tab.
///
/// Unlike the rest of the panel, mutations here apply to the **global**
/// settings signal immediately and persist via `save_async` — they carry side
/// effects a discardable draft can't hold (keychain writes, cache purges,
/// sync-loop kicks), and the sync coroutine reads the global signal. The
/// panel's resync effect folds the change back into the local draft.
#[component]
pub(super) fn CalendarSubscriptionsSection() -> Element {
    use crate::settings::CalendarSource;
    use crate::todo::calendar_sync::CalendarSyncMsg;

    let mut settings = use_context::<Signal<Settings>>();
    let settings_manager = use_context::<Signal<SettingsManager>>();
    let mut secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();
    let sync = use_context::<Coroutine<CalendarSyncMsg>>();
    let save_error = use_context::<crate::components::shared::SaveErrorContext>().0;

    // Add-ICS form state.
    let mut ics_form_open = use_signal(|| false);
    let mut ics_name = use_signal(String::new);
    let mut ics_url = use_signal(String::new);
    let mut ics_error = use_signal(|| Option::<String>::None);

    // Add-Google form state: pick a Composio profile, then a calendar. When
    // the calendar listing fails, fall back to a manually typed calendar id.
    let mut gcal_form_open = use_signal(|| false);
    let mut gcal_profile_id = use_signal(|| Option::<String>::None);
    let mut gcal_loading = use_signal(|| false);
    let mut gcal_calendars = use_signal(|| Option::<Result<Vec<(String, String)>, String>>::None);
    let mut gcal_selected = use_signal(|| Option::<String>::None);
    let mut gcal_manual_id = use_signal(|| "primary".to_string());

    let persist = move || {
        settings_manager
            .peek()
            .save_async(settings.peek().clone(), Some(save_error));
    };

    let mut reset_gcal_form = move || {
        gcal_form_open.set(false);
        gcal_profile_id.set(None);
        gcal_loading.set(false);
        gcal_calendars.set(None);
        gcal_selected.set(None);
        gcal_manual_id.set("primary".to_string());
    };

    // Fetch the chosen profile's calendar list. Loading and error states are
    // deliberately simple; an error opens the manual calendar-id fallback.
    let mut fetch_calendars = move |profile_id: String| {
        gcal_loading.set(true);
        gcal_calendars.set(None);
        gcal_selected.set(None);
        let settings_snapshot = settings.peek().clone();
        spawn(async move {
            let result = async {
                mcp_manager
                    .read()
                    .ensure_native_client_for_profile(&profile_id, &settings_snapshot)
                    .await?;
                let client = mcp_manager
                    .read()
                    .composio_client_for_profile(&profile_id)
                    .await?;
                crate::todo::composio_calendar::list_google_calendars(&client).await
            }
            .await;
            if let Ok(list) = &result {
                gcal_selected.set(list.first().map(|(id, _)| id.clone()));
            }
            gcal_calendars.set(Some(result));
            gcal_loading.set(false);
        });
    };

    let subs = settings.read().planner_calendar_subscriptions.clone();
    let profiles: Vec<(String, String)> = settings
        .read()
        .composio_profiles
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let has_profiles = !profiles.is_empty();

    rsx! {
        div {
            class: "border border-subtle rounded-lg mb-4",
            div {
                class: "p-4 bg-section rounded-lg",
                div {
                    class: "mb-3 flex items-center justify-between",
                    h3 { class: "text-[11px] uppercase tracking-[0.08em] text-primary-300 font-medium", "Calendar Subscriptions" }
                    button {
                        class: "rounded border border-subtle px-2 py-1 text-xs text-fg-muted hover:bg-white/5 hover:text-fg",
                        title: "Fetch every enabled subscription now",
                        onclick: move |_| sync.send(CalendarSyncMsg::SyncNow),
                        "Sync now"
                    }
                }
                p { class: "text-sm text-fg-muted mb-4",
                    "Read-only calendars mirrored onto the Today timeline. Busy meetings count as planned time in the day's capacity; nothing is ever written back."
                }

                if subs.is_empty() {
                    p { class: "mb-3 text-xs text-fg-muted italic", "No calendars subscribed yet." }
                } else {
                    div {
                        class: "mb-3 flex flex-col gap-2",
                        for sub in subs {
                            CalendarSubscriptionRow { key: "{sub.id}", sub: sub.clone() }
                        }
                    }
                }

                div {
                    class: "flex gap-2",
                    button {
                        class: "rounded border border-dashed border-subtle px-3 py-1.5 text-xs text-fg-muted hover:bg-white/5 hover:text-fg",
                        onclick: move |_| {
                            let open = *ics_form_open.peek();
                            ics_form_open.set(!open);
                            ics_error.set(None);
                        },
                        "+ ICS Feed"
                    }
                    button {
                        class: "rounded border border-dashed border-subtle px-3 py-1.5 text-xs text-fg-muted hover:bg-white/5 hover:text-fg",
                        onclick: move |_| {
                            let open = *gcal_form_open.peek();
                            if open {
                                reset_gcal_form();
                            } else {
                                gcal_form_open.set(true);
                                // A single profile needs no picking — load its
                                // calendars straight away.
                                if profiles.len() == 1 {
                                    let id = profiles[0].0.clone();
                                    gcal_profile_id.set(Some(id.clone()));
                                    fetch_calendars(id);
                                }
                            }
                        },
                        "+ Google Calendar"
                    }
                }

                if ics_form_open() {
                    div {
                        class: "mt-3 flex flex-col gap-2 rounded border border-subtle bg-input/30 p-3",
                        p { class: "text-xs text-fg-muted",
                            "Paste the calendar's secret ICS address (Google: Settings → \"Secret address in iCal format\"). The URL is stored in your keychain."
                        }
                        input {
                            r#type: "text",
                            class: "w-full rounded border border-subtle bg-input px-3 py-1.5 text-sm text-fg",
                            placeholder: "Name (e.g. Work)",
                            value: "{ics_name}",
                            oninput: move |e| ics_name.set(e.value()),
                        }
                        input {
                            r#type: "text",
                            class: "w-full rounded border border-subtle bg-input px-3 py-1.5 text-sm text-fg font-mono",
                            placeholder: "https://… or webcal://…",
                            value: "{ics_url}",
                            oninput: move |e| {
                                ics_url.set(e.value());
                                ics_error.set(None);
                            },
                        }
                        if let Some(err) = ics_error() {
                            p { class: "text-xs text-red-400", "{err}" }
                        }
                        div {
                            class: "flex justify-end gap-2",
                            button {
                                class: "px-3 py-1 text-xs text-fg-muted hover:text-fg",
                                onclick: move |_| {
                                    ics_form_open.set(false);
                                    ics_name.set(String::new());
                                    ics_url.set(String::new());
                                    ics_error.set(None);
                                },
                                "Cancel"
                            }
                            button {
                                class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm",
                                onclick: move |_| {
                                    let url = ics_url.peek().trim().to_string();
                                    if let Err(e) = validate_ics_url(&url) {
                                        ics_error.set(Some(e));
                                        return;
                                    }
                                    let name = {
                                        let n = ics_name.peek().trim().to_string();
                                        if n.is_empty() { "Calendar feed".to_string() } else { n }
                                    };
                                    let (id, biometric) = {
                                        let mut s = settings.write();
                                        let color =
                                            pick_default_color(s.planner_calendar_subscriptions.len());
                                        let id = s.add_calendar_subscription(
                                            &name,
                                            color,
                                            CalendarSource::Ics {},
                                        );
                                        (id, s.use_biometric_storage())
                                    };
                                    // The URL is the secret; settings holds only
                                    // the id. set_cal_url also registers the key
                                    // in the cal_keys_index for startup discovery.
                                    if let Err(e) =
                                        secret_manager.write().set_cal_url(&id, url, biometric)
                                    {
                                        // No URL, no subscription — roll back
                                        // rather than leave one that can never sync.
                                        settings.write().remove_calendar_subscription(&id);
                                        ics_error.set(Some(format!(
                                            "Could not save the URL to the keychain: {}",
                                            e
                                        )));
                                        return;
                                    }
                                    persist();
                                    sync.send(CalendarSyncMsg::SyncNow);
                                    ics_form_open.set(false);
                                    ics_name.set(String::new());
                                    ics_url.set(String::new());
                                    ics_error.set(None);
                                },
                                "Add Feed"
                            }
                        }
                    }
                }

                if gcal_form_open() {
                    div {
                        class: "mt-3 flex flex-col gap-2 rounded border border-subtle bg-input/30 p-3",
                        if !has_profiles {
                            p { class: "text-xs text-fg-muted",
                                "No Composio profiles are set up. Connect Google Calendar in the MCP Marketplace (Tools → Marketplace → Composio) first, then come back here to subscribe."
                            }
                        } else {
                            label { class: "text-xs font-medium text-fg-muted", "Composio profile" }
                            select {
                                class: "w-full rounded border border-subtle bg-input px-2 py-1.5 text-sm text-fg",
                                onchange: move |e| {
                                    let id = e.value();
                                    if id.is_empty() {
                                        gcal_profile_id.set(None);
                                        gcal_calendars.set(None);
                                    } else {
                                        gcal_profile_id.set(Some(id.clone()));
                                        fetch_calendars(id);
                                    }
                                },
                                option {
                                    value: "",
                                    selected: gcal_profile_id.read().is_none(),
                                    "Choose a profile…"
                                }
                                for (pid, pname) in profiles.clone() {
                                    option {
                                        key: "{pid}",
                                        value: "{pid}",
                                        selected: gcal_profile_id.read().as_deref() == Some(pid.as_str()),
                                        "{pname}"
                                    }
                                }
                            }

                            if gcal_loading() {
                                p { class: "text-xs text-fg-muted italic", "Loading calendars…" }
                            }
                            match gcal_calendars() {
                                Some(Ok(list)) => rsx! {
                                    label { class: "text-xs font-medium text-fg-muted", "Calendar" }
                                    select {
                                        class: "w-full rounded border border-subtle bg-input px-2 py-1.5 text-sm text-fg",
                                        onchange: move |e| gcal_selected.set(Some(e.value())),
                                        for (cid, summary) in list.clone() {
                                            option {
                                                key: "{cid}",
                                                value: "{cid}",
                                                selected: gcal_selected.read().as_deref() == Some(cid.as_str()),
                                                "{summary}"
                                            }
                                        }
                                    }
                                },
                                Some(Err(err)) => rsx! {
                                    p {
                                        class: "text-xs text-red-400",
                                        title: "{err}",
                                        "Couldn't list calendars: {err}"
                                    }
                                    label { class: "text-xs font-medium text-fg-muted", "Calendar ID" }
                                    input {
                                        r#type: "text",
                                        class: "w-full rounded border border-subtle bg-input px-3 py-1.5 text-sm text-fg font-mono",
                                        placeholder: "primary",
                                        value: "{gcal_manual_id}",
                                        oninput: move |e| gcal_manual_id.set(e.value()),
                                    }
                                },
                                None => rsx! {},
                            }
                        }
                        div {
                            class: "flex justify-end gap-2",
                            button {
                                class: "px-3 py-1 text-xs text-fg-muted hover:text-fg",
                                onclick: move |_| reset_gcal_form(),
                                "Cancel"
                            }
                            if has_profiles {
                                button {
                                    class: "bg-btn-primary hover:bg-btn-primary-hover text-fg rounded px-3 py-1.5 text-sm disabled:opacity-50",
                                    disabled: gcal_profile_id.read().is_none(),
                                    onclick: move |_| {
                                        let Some(profile_id) = gcal_profile_id.peek().clone() else {
                                            return;
                                        };
                                        // Picked calendar when the listing worked;
                                        // the manual id as the fallback.
                                        let (calendar_id, summary) = match &*gcal_calendars.peek() {
                                            Some(Ok(list)) => {
                                                let picked = gcal_selected
                                                    .peek()
                                                    .clone()
                                                    .or_else(|| list.first().map(|(id, _)| id.clone()));
                                                match picked {
                                                    Some(id) => {
                                                        let name = list
                                                            .iter()
                                                            .find(|(cid, _)| *cid == id)
                                                            .map(|(_, s)| s.clone());
                                                        (id, name)
                                                    }
                                                    None => return,
                                                }
                                            }
                                            _ => {
                                                let manual = gcal_manual_id.peek().trim().to_string();
                                                if manual.is_empty() {
                                                    return;
                                                }
                                                (manual, None)
                                            }
                                        };
                                        let name =
                                            summary.unwrap_or_else(|| "Google Calendar".to_string());
                                        {
                                            let mut s = settings.write();
                                            let color = pick_default_color(
                                                s.planner_calendar_subscriptions.len(),
                                            );
                                            s.add_calendar_subscription(
                                                &name,
                                                color,
                                                CalendarSource::Composio {
                                                    profile_id,
                                                    calendar_id,
                                                },
                                            );
                                        }
                                        persist();
                                        sync.send(CalendarSyncMsg::SyncNow);
                                        reset_gcal_form();
                                    },
                                    "Add Calendar"
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
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn validate_ics_url_accepts_http_https_and_webcal() {
        assert!(validate_ics_url("https://example.com/cal.ics").is_ok());
        assert!(validate_ics_url("http://example.com/cal.ics").is_ok());
        assert!(validate_ics_url("webcal://example.com/cal.ics").is_ok());
        // Case-insensitive scheme, surrounding whitespace tolerated.
        assert!(validate_ics_url("  WEBCAL://Example.com/feed  ").is_ok());
        assert!(validate_ics_url("HTTPS://example.com/basic.ics").is_ok());
        // A bare host is enough — some feeds have no path.
        assert!(validate_ics_url("https://example.com").is_ok());
    }

    #[test]
    fn validate_ics_url_rejects_junk() {
        assert!(validate_ics_url("").is_err());
        assert!(validate_ics_url("   ").is_err());
        assert!(validate_ics_url("example.com/cal.ics").is_err());
        assert!(validate_ics_url("ftp://example.com/cal.ics").is_err());
        assert!(validate_ics_url("file:///etc/passwd").is_err());
        assert!(validate_ics_url("just some words").is_err());
        // Scheme with no host.
        assert!(validate_ics_url("https://").is_err());
        assert!(validate_ics_url("webcal:///path-only").is_err());
        assert!(validate_ics_url("https://?query").is_err());
    }

    #[test]
    fn default_palette_cycles_by_index() {
        let n = CALENDAR_COLOR_PALETTE.len();
        assert!(n >= 6, "palette should offer a real choice");
        for i in 0..n {
            assert_eq!(pick_default_color(i), CALENDAR_COLOR_PALETTE[i]);
        }
        // Wraps around instead of running out.
        assert_eq!(pick_default_color(n), CALENDAR_COLOR_PALETTE[0]);
        assert_eq!(pick_default_color(n + 2), CALENDAR_COLOR_PALETTE[2]);
        // Every palette entry is a well-formed #rrggbb hex — the timeline
        // appends an alpha byte to it.
        for c in CALENDAR_COLOR_PALETTE {
            assert_eq!(c.len(), 7, "{}", c);
            assert!(c.starts_with('#'));
            assert!(c[1..].chars().all(|ch| ch.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn friendly_sync_time_scales_with_age() {
        let now = Utc.with_ymd_and_hms(2026, 8, 12, 12, 0, 0).unwrap();
        assert_eq!(friendly_sync_time(now, now), "just now");
        assert_eq!(
            friendly_sync_time(now - Duration::seconds(30), now),
            "just now"
        );
        assert_eq!(
            friendly_sync_time(now - Duration::minutes(5), now),
            "5m ago"
        );
        assert_eq!(friendly_sync_time(now - Duration::hours(3), now), "3h ago");
        // Older than a day: falls through to the planner's friendly date
        // (exact wording covered by views::friendly_date's own tests).
        let old = friendly_sync_time(now - Duration::days(30), now);
        assert!(!old.contains("ago"), "{}", old);
        // A clock skewed slightly into the future must not underflow.
        assert_eq!(
            friendly_sync_time(now + Duration::seconds(10), now),
            "just now"
        );
    }
}
