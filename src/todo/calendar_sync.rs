//! Calendar subscription sync: fetch → cache → reconcile → materialize.
//!
//! Phase 1 built the skeleton (loop, cache reconciler, materializer); Phase 2
//! added the ICS transport (`Fetcher::Ics`, backed by `todo::ics`) with
//! conditional-GET support; Phase 3 added the Composio Google Calendar
//! transport (`Fetcher::Composio`, backed by `todo::composio_calendar`).
//!
//! Pipeline per subscription:
//!   1. fetch the window's events (async, network) — **all awaits happen here**
//!   2. reconcile against the cached rows (pure)
//!   3. write cache upserts/deletes (synchronous SQLite)
//!   4. re-materialize the window into `PlannerState.blocks` (in-memory + store)
//!
//! P-010: the session-store mutex is never held across an `.await` — fetches
//! complete first, then every store call is synchronous.

// Dioxus Signal types are held across .await — not real locks, just Dioxus marker types.
#![allow(clippy::await_holding_invalid_type)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::mcp::composio_client::ComposioClient;
use crate::settings::{CalendarSource, CalendarSubscription, Settings};

use super::model::{BlockSource, CalendarEvent, TimeBlock};
use super::{store, PlannerState};

/// How far the materialization window extends either side of today, in
/// **local** calendar days.
pub const WINDOW_DAYS: i64 = 14;

/// How often the background loop re-syncs.
const SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

// ── Fetcher ─────────────────────────────────────────────────────────────────

/// What one fetch produced.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchOutcome {
    /// A fresh feed body was parsed. `etag` / `last_modified` are the
    /// validators the server offered for the next conditional GET.
    Fetched {
        events: Vec<CalendarEvent>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// The server answered `304 Not Modified` — the cache is already current;
    /// reconcile and materialize can be skipped.
    NotModified,
}

/// A source of calendar events for one subscription over a UTC window.
///
/// Enum dispatch rather than a trait object: the crate has no async-trait
/// dependency, and a closed set of transports (ICS, Composio, test fake) is
/// exactly what an enum models.
#[derive(Clone)]
pub enum Fetcher {
    /// An ICS/webcal feed over HTTPS. The URL is a secret (feeds embed access
    /// tokens); the caller resolves it from the keychain — this variant never
    /// touches the secret manager itself.
    Ics { url: String },
    /// Google Calendar via the profile's Composio client. The Arc is cloned
    /// out of the MCP servers map *before* any await (P-010) — this variant
    /// never touches `McpManager` locks itself.
    Composio {
        client: Arc<ComposioClient>,
        calendar_id: String,
    },
    /// Test double: returns its canned result.
    #[allow(dead_code)] // constructed by tests
    Fake(Result<Vec<CalendarEvent>, String>),
}

// Manual: `ComposioClient` has no `Debug` impl.
impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fetcher::Ics { url } => f.debug_struct("Ics").field("url", url).finish(),
            Fetcher::Composio { calendar_id, .. } => f
                .debug_struct("Composio")
                .field("calendar_id", calendar_id)
                .finish_non_exhaustive(),
            Fetcher::Fake(result) => f.debug_tuple("Fake").field(result).finish(),
        }
    }
}

impl Fetcher {
    /// `prev` supplies the stored HTTP validators for conditional GETs.
    pub async fn fetch(
        &self,
        sub: &CalendarSubscription,
        window: (DateTime<Utc>, DateTime<Utc>),
        prev: &SyncState,
    ) -> Result<FetchOutcome, String> {
        match self {
            Fetcher::Ics { url } => {
                super::ics::fetch_ics(
                    url,
                    &sub.id,
                    window,
                    prev.etag.as_deref(),
                    prev.last_modified.as_deref(),
                )
                .await
            }
            Fetcher::Composio {
                client,
                calendar_id,
            } => super::composio_calendar::fetch_composio(client, calendar_id, &sub.id, window)
                .await,
            Fetcher::Fake(result) => result.clone().map(|events| FetchOutcome::Fetched {
                events,
                etag: None,
                last_modified: None,
            }),
        }
    }
}

/// Resolve the fetcher for a subscription's transport.
///
/// `ics_url` is the subscription's feed URL as resolved from the keychain by
/// the caller (near the coroutine, where the `SecretManager` lives) — `None`
/// when the secret is missing. `composio_client` is the subscription profile's
/// Composio client Arc as resolved from the `McpManager` by the caller (the
/// Arc is cloned out with no MCP lock held past resolution, P-010) — `None`
/// when the profile has no connected client. `Err` carries the honest reason
/// the subscription cannot sync, surfaced as its `last_error`.
pub fn resolve_fetcher(
    sub: &CalendarSubscription,
    ics_url: Option<&str>,
    composio_client: Option<Arc<ComposioClient>>,
) -> Result<Fetcher, String> {
    match &sub.source {
        CalendarSource::Ics {} => match ics_url {
            Some(url) if !url.trim().is_empty() => Ok(Fetcher::Ics {
                url: super::ics::normalize_ics_url(url),
            }),
            _ => Err(
                "No feed URL found in the keychain for this calendar subscription.".to_string(),
            ),
        },
        CalendarSource::Composio { calendar_id, .. } => match composio_client {
            Some(client) => Ok(Fetcher::Composio {
                client,
                calendar_id: calendar_id.clone(),
            }),
            None => Err(
                "No connected Google Calendar account is available for this subscription — \
                 reconnect its Composio profile, then sync again."
                    .to_string(),
            ),
        },
    }
}

// ── Sync state (meta table) ─────────────────────────────────────────────────

/// Per-subscription sync bookkeeping, serialized into the sessions.db `meta`
/// table under `cal_sync_<subscription_id>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub last_synced_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// HTTP `ETag` from the last successful ICS fetch, replayed as
    /// `If-None-Match`. Serde defaults keep Phase 1 payloads loading.
    #[serde(default)]
    pub etag: Option<String>,
    /// HTTP `Last-Modified` from the last successful ICS fetch, replayed as
    /// `If-Modified-Since`.
    #[serde(default)]
    pub last_modified: Option<String>,
}

fn sync_state_key(subscription_id: &str) -> String {
    format!("cal_sync_{}", subscription_id)
}

pub fn load_sync_state(subscription_id: &str) -> SyncState {
    crate::session_store::meta_get(&sync_state_key(subscription_id))
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_sync_state(subscription_id: &str, state: &SyncState) {
    match serde_json::to_string(state) {
        Ok(raw) => {
            if let Err(e) = crate::session_store::meta_set(&sync_state_key(subscription_id), &raw)
            {
                tracing::error!("Failed to persist calendar sync state: {}", e);
            }
        }
        Err(e) => tracing::error!("Failed to serialize calendar sync state: {}", e),
    }
}

// ── Reconciler ──────────────────────────────────────────────────────────────

/// Identity of one event occurrence: `(subscription_id, uid, start)`.
pub type EventKey = (String, String, DateTime<Utc>);

fn event_key(e: &CalendarEvent) -> EventKey {
    (e.subscription_id.clone(), e.uid.clone(), e.start)
}

/// The cache writes one sync pass owes for one subscription.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconciliation {
    /// New or changed events (unchanged events are skipped — no writes).
    pub upserts: Vec<CalendarEvent>,
    /// Occurrences that vanished from the feed.
    pub deletes: Vec<EventKey>,
}

/// Diff one subscription's cached events against a fresh fetch.
///
/// Pure: both inputs must belong to the same subscription; the caller scopes
/// them. Keyed by `(subscription_id, uid, start)`, so a moved occurrence shows
/// up as delete + insert — correct for recurring events, where "the 10:00
/// slot moved" and "this Tuesday's instance was cancelled" are the same shape.
pub fn reconcile(cached: &[CalendarEvent], fetched: &[CalendarEvent]) -> Reconciliation {
    let cached_by_key: HashMap<EventKey, &CalendarEvent> =
        cached.iter().map(|e| (event_key(e), e)).collect();
    let fetched_keys: HashSet<EventKey> = fetched.iter().map(event_key).collect();

    let upserts = fetched
        .iter()
        .filter(|f| cached_by_key.get(&event_key(f)) != Some(f))
        .cloned()
        .collect();
    let deletes = cached
        .iter()
        .map(event_key)
        .filter(|k| !fetched_keys.contains(k))
        .collect();

    Reconciliation { upserts, deletes }
}

// ── Materializer ────────────────────────────────────────────────────────────

/// Stable block id for an event occurrence. Derived, not random: re-running
/// the materializer must upsert the same block, never mint a sibling.
pub fn external_block_id(subscription_id: &str, uid: &str, start: &DateTime<Utc>) -> String {
    format!("calblk_{}_{}_{}", subscription_id, uid, start.timestamp())
}

/// The block changes one materialization pass produced, for persistence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Materialization {
    pub upserted: Vec<TimeBlock>,
    pub removed: Vec<TimeBlock>,
}

/// The materialization window around `today` as **local** calendar days.
pub fn window_days(today: NaiveDate) -> (NaiveDate, NaiveDate) {
    (today - Duration::days(WINDOW_DAYS), today + Duration::days(WINDOW_DAYS))
}

/// The fetch window as UTC instants: local midnight opening the first day
/// through local midnight closing the last. Computed in LOCAL days first —
/// deriving it from `Utc::now()` shifts the edges by the UTC offset (the
/// `blocks_on` trap).
pub fn window_bounds(today: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let (first, last) = window_days(today);
    (window_edge_utc(first), window_edge_utc(last + Duration::days(1)))
}

/// Local midnight of a window edge as UTC, DST-gap tolerant: in timezones that
/// spring forward AT midnight (Santiago, Havana, Beirut) `00:00` doesn't exist
/// on transition day and a bare `.earliest().expect(..)` would panic the sync
/// coroutine on every pass. `ics::local_midnight` already slides such gaps
/// forward an hour; if even that fails, fall back to UTC midnight rather than
/// panic.
fn window_edge_utc(date: NaiveDate) -> DateTime<Utc> {
    use chrono::TimeZone;
    super::ics::local_midnight(date).unwrap_or_else(|| {
        Utc.from_utc_datetime(&date.and_time(chrono::NaiveTime::MIN))
    })
}

/// Mirror the cached events into `PlannerState.blocks` as read-only external
/// blocks, and garbage-collect external blocks whose event vanished or whose
/// subscription is disabled/removed.
///
/// - Window: local days `today ± WINDOW_DAYS`, compared via
///   `.with_timezone(&Local).date_naive()` (the `blocks_on` trap).
/// - All-day events stay cached but are NOT materialized in this phase — a
///   24h block would wreck `block_geometry`; Phase 4 renders them as a banner.
/// - Only blocks carrying a `subscription_id` are GC'd here: legacy external
///   blocks (`subscription_id: None`) predate subscriptions and are not ours
///   to reap. Manual/Auto blocks are never touched.
///
/// Pure with respect to storage — the caller persists via
/// [`persist_materialization`] (mirrors the handlers' `persist: bool` split).
pub fn materialize_window(
    state: &mut PlannerState,
    cached: &[CalendarEvent],
    enabled_subscriptions: &HashSet<String>,
    today: NaiveDate,
) -> Materialization {
    let (first_day, last_day) = window_days(today);

    // What should exist on the timeline after this pass.
    let mut desired: HashMap<String, &CalendarEvent> = HashMap::new();
    for event in cached {
        if event.all_day
            || event.end <= event.start
            || !enabled_subscriptions.contains(&event.subscription_id)
        {
            continue;
        }
        let local_day = event.start.with_timezone(&chrono::Local).date_naive();
        if local_day < first_day || local_day > last_day {
            continue;
        }
        desired.insert(
            external_block_id(&event.subscription_id, &event.uid, &event.start),
            event,
        );
    }

    let mut out = Materialization::default();

    // Remove sync-owned external blocks that no longer correspond to a
    // desired event (vanished, disabled subscription, or out of window and
    // previously materialized).
    state.blocks.retain(|b| {
        let owned = matches!(
            &b.source,
            BlockSource::External {
                subscription_id: Some(_),
                ..
            }
        );
        if owned && !desired.contains_key(&b.id) {
            out.removed.push(b.clone());
            false
        } else {
            true
        }
    });

    // Upsert the desired blocks, skipping ones already in sync (no writes).
    for (id, event) in desired {
        let block = TimeBlock {
            id,
            todo_id: None,
            title: event.title.clone(),
            start: event.start,
            end: event.end,
            source: BlockSource::External {
                uid: event.uid.clone(),
                subscription_id: Some(event.subscription_id.clone()),
                url: event.url.clone(),
                busy: event.busy,
                tentative: event.tentative,
            },
        };
        match state.blocks.iter_mut().find(|b| b.id == block.id) {
            Some(existing) => {
                if *existing != block {
                    *existing = block.clone();
                    out.upserted.push(block);
                }
            }
            None => {
                state.blocks.push(block.clone());
                out.upserted.push(block);
            }
        }
    }

    out
}

/// Write a materialization's block changes through the store, like the UI's
/// `persist_block` does.
pub fn persist_materialization(m: &Materialization) {
    for block in &m.upserted {
        if let Err(e) = store::save_block(block) {
            tracing::error!("calendar sync: failed to save block {}: {}", block.id, e);
        }
    }
    for block in &m.removed {
        if let Err(e) = store::delete_block(&block.id) {
            tracing::error!("calendar sync: failed to delete block {}: {}", block.id, e);
        }
    }
}

// ── One sync pass ───────────────────────────────────────────────────────────

/// Fetch and cache one subscription's window. All awaits complete before any
/// store write (P-010). Returns the updated sync state.
pub async fn sync_subscription(
    sub: &CalendarSubscription,
    fetcher: &Fetcher,
    window: (DateTime<Utc>, DateTime<Utc>),
    now: DateTime<Utc>,
) -> SyncState {
    // Prior state (read synchronously, before the network await) feeds the
    // conditional GET and survives a failed fetch.
    let prev = load_sync_state(&sub.id);

    // 1. Network, fully awaited before any store access.
    let (fetched, etag, last_modified) = match fetcher.fetch(sub, window, &prev).await {
        Ok(FetchOutcome::Fetched {
            events,
            etag,
            last_modified,
        }) => (events, etag, last_modified),
        Ok(FetchOutcome::NotModified) => {
            // The cache is current — skip reconcile, but record the check.
            return SyncState {
                last_synced_at: Some(now),
                last_error: None,
                ..prev
            };
        }
        Err(e) => {
            tracing::warn!("Calendar sync failed for '{}': {}", sub.name, e);
            return SyncState {
                last_error: Some(e),
                ..prev
            };
        }
    };
    // Defensive scope: a fetcher must only ever return its own
    // subscription's events; a mislabeled row would leak into another
    // subscription's cache key-space.
    let fetched: Vec<CalendarEvent> = fetched
        .into_iter()
        .filter(|e| e.subscription_id == sub.id)
        .collect();

    // 2–3. Reconcile against the cache and write the diff, synchronously.
    let cached = match store::load_calendar_events(Some(&sub.id)) {
        Ok(events) => events,
        Err(e) => {
            tracing::error!("Failed to load calendar cache for '{}': {}", sub.name, e);
            return SyncState {
                last_error: Some(e),
                ..prev
            };
        }
    };
    // Only reconcile within the fetched window: events cached outside it were
    // not re-fetched, so their absence from `fetched` says nothing.
    let cached_in_window: Vec<CalendarEvent> = cached
        .into_iter()
        .filter(|e| e.start >= window.0 && e.start < window.1)
        .collect();
    let diff = reconcile(&cached_in_window, &fetched);
    for event in &diff.upserts {
        if let Err(e) = store::save_calendar_event(event) {
            tracing::error!("Failed to cache calendar event {}: {}", event.uid, e);
        }
    }
    for (sub_id, uid, start) in &diff.deletes {
        if let Err(e) = store::delete_calendar_event(sub_id, uid, start) {
            tracing::error!("Failed to delete cached calendar event {}: {}", uid, e);
        }
    }

    SyncState {
        last_synced_at: Some(now),
        last_error: None,
        etag,
        last_modified,
    }
}

/// One full pass over every subscription: fetch + cache each enabled one,
/// then re-materialize the window into the planner.
///
/// `ics_urls` maps subscription id → keychain-resolved feed URL, and
/// `composio_clients` maps subscription id → the profile's Composio client
/// Arc; the caller resolves both (the fetchers never touch the secret manager
/// or the `McpManager`). A subscription whose transport cannot be resolved
/// records the honest error instead of pretending to have synced.
pub async fn run_sync_pass(
    planner: &mut dioxus::prelude::Signal<PlannerState>,
    subscriptions: &[CalendarSubscription],
    ics_urls: &HashMap<String, String>,
    composio_clients: &HashMap<String, Arc<ComposioClient>>,
    today: NaiveDate,
) {
    let window = window_bounds(today);
    let now = Utc::now();

    for sub in subscriptions.iter().filter(|s| s.enabled) {
        let state = match resolve_fetcher(
            sub,
            ics_urls.get(&sub.id).map(String::as_str),
            composio_clients.get(&sub.id).cloned(),
        ) {
            Ok(fetcher) => sync_subscription(sub, &fetcher, window, now).await,
            Err(reason) => SyncState {
                last_error: Some(reason),
                ..load_sync_state(&sub.id)
            },
        };
        save_sync_state(&sub.id, &state);
    }

    // Materialize from the full cache in one pass — disabled/removed
    // subscriptions fall out here because they are absent from `enabled`.
    let cached = match store::load_calendar_events(None) {
        Ok(events) => events,
        Err(e) => {
            tracing::error!("Failed to load calendar cache for materialization: {}", e);
            return;
        }
    };
    let enabled: HashSet<String> = subscriptions
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id.clone())
        .collect();

    use dioxus::prelude::*;
    let changes = materialize_window(&mut planner.write(), &cached, &enabled, today);
    if !changes.upserted.is_empty() || !changes.removed.is_empty() {
        tracing::info!(
            "Calendar sync materialized {} block(s), removed {}",
            changes.upserted.len(),
            changes.removed.len()
        );
    }
    persist_materialization(&changes);
}

// ── Coroutine wiring ────────────────────────────────────────────────────────

/// Resolve each enabled Composio subscription's client Arc from the
/// `McpManager`, keyed by subscription id. Profiles resolve once even when
/// several subscriptions share one. The manager acquires and releases its own
/// locks inside each call; only the cloned Arcs leave here (P-010) — a
/// missing/failed profile simply has no entry, and `resolve_fetcher` turns
/// that into the subscription's honest `last_error`.
async fn resolve_composio_clients(
    subscriptions: &[CalendarSubscription],
    settings: &dioxus::prelude::Signal<Settings>,
    mcp_manager: &dioxus::prelude::Signal<crate::mcp::manager::McpManager>,
) -> HashMap<String, Arc<ComposioClient>> {
    use dioxus::prelude::*;

    let wanted: Vec<(String, String)> = subscriptions
        .iter()
        .filter(|s| s.enabled)
        .filter_map(|s| match &s.source {
            CalendarSource::Composio { profile_id, .. } => {
                Some((s.id.clone(), profile_id.clone()))
            }
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return HashMap::new();
    }

    // Snapshot settings before the awaits (ensure_native_client_for_profile
    // needs the profile's connection details).
    let settings_snapshot = settings.peek().clone();

    let mut by_profile: HashMap<String, Option<Arc<ComposioClient>>> = HashMap::new();
    let mut out: HashMap<String, Arc<ComposioClient>> = HashMap::new();
    for (sub_id, profile_id) in wanted {
        if !by_profile.contains_key(&profile_id) {
            // Idempotent: returns immediately when the profile's client is
            // already in the servers map.
            if let Err(e) = mcp_manager
                .read()
                .ensure_native_client_for_profile(&profile_id, &settings_snapshot)
                .await
            {
                tracing::warn!(
                    "Calendar sync: could not initialize Composio client for profile '{}': {}",
                    profile_id,
                    e
                );
            }
            let client = match mcp_manager
                .read()
                .composio_client_for_profile(&profile_id)
                .await
            {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::warn!(
                        "Calendar sync: no Composio client for profile '{}': {}",
                        profile_id,
                        e
                    );
                    None
                }
            };
            by_profile.insert(profile_id.clone(), client);
        }
        if let Some(Some(client)) = by_profile.get(&profile_id) {
            out.insert(sub_id, client.clone());
        }
    }
    out
}

/// Messages the calendar sync loop accepts.
pub enum CalendarSyncMsg {
    /// Sync immediately (settings changes, a future "Sync now" button).
    #[allow(dead_code)] // sender is the Phase 4 settings UI's "Sync now" button
    SyncNow,
}

/// Background sync loop, modeled on `use_summarization_scheduler`: an
/// immediate pass on launch, then every [`SYNC_INTERVAL`] or on `SyncNow`.
/// The whole loop is gated on `settings.planner_enabled`.
pub fn use_calendar_sync() {
    use dioxus::prelude::*;
    use futures_util::StreamExt;

    let planner = use_context::<Signal<PlannerState>>();
    let settings = use_context::<Signal<Settings>>();
    let secret_manager = use_context::<Signal<crate::secret_manager::SecretManager>>();
    let mcp_manager = use_context::<Signal<crate::mcp::manager::McpManager>>();

    let coroutine = use_coroutine(move |mut rx: UnboundedReceiver<CalendarSyncMsg>| {
        let mut planner = planner.to_owned();
        let settings = settings.to_owned();
        let secret_manager = secret_manager.to_owned();
        let mcp_manager = mcp_manager.to_owned();
        async move {
            loop {
                let (enabled, subscriptions) = {
                    let s = settings.read();
                    (s.planner_enabled, s.planner_calendar_subscriptions.clone())
                };
                if enabled {
                    // Resolve ICS feed URLs from the keychain-backed secret
                    // cache. Scoped read: the guard drops before any await.
                    let ics_urls: HashMap<String, String> = {
                        use crate::SecretManagerTrait;
                        let sm = secret_manager.read();
                        subscriptions
                            .iter()
                            .filter(|s| s.enabled && matches!(s.source, CalendarSource::Ics {}))
                            .filter_map(|s| {
                                sm.get_cal_url(&s.id).map(|url| (s.id.clone(), url.clone()))
                            })
                            .collect()
                    };
                    // Resolve each Composio subscription's client Arc up
                    // front. The manager methods lock and release internally;
                    // only cloned Arcs cross into the fetch awaits (P-010).
                    let composio_clients =
                        resolve_composio_clients(&subscriptions, &settings, &mcp_manager).await;
                    let today = chrono::Local::now().date_naive();
                    run_sync_pass(&mut planner, &subscriptions, &ics_urls, &composio_clients, today)
                        .await;
                }

                // Sleep until the next tick, waking early on SyncNow.
                match tokio::time::timeout(SYNC_INTERVAL, rx.next()).await {
                    Ok(Some(CalendarSyncMsg::SyncNow)) => continue,
                    Ok(None) => {
                        tracing::info!("Calendar sync loop shutting down.");
                        break;
                    }
                    Err(_) => continue, // interval elapsed
                }
            }
        }
    });

    use_context_provider(|| coroutine);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// A UTC instant at `hour`:00 **local** time on `day`, mirroring how the
    /// planner builds instants everywhere; keeps tests timezone-independent.
    fn local_instant(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        chrono::Local
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn event(sub: &str, uid: &str, start: DateTime<Utc>, minutes: i64) -> CalendarEvent {
        CalendarEvent {
            uid: uid.into(),
            subscription_id: sub.into(),
            title: format!("Event {}", uid),
            start,
            end: start + Duration::minutes(minutes),
            all_day: false,
            url: Some(format!("https://cal.example/{}", uid)),
            location: None,
            busy: true,
            tentative: false,
        }
    }

    // ── Reconciler ──────────────────────────────────────────────────────────

    #[test]
    fn reconcile_classifies_added_changed_removed_unchanged() {
        let t = local_instant(date("2026-08-12"), 9);
        let unchanged = event("s1", "keep", t, 30);
        let mut changed_old = event("s1", "changed", t + Duration::hours(1), 30);
        let mut changed_new = changed_old.clone();
        changed_new.title = "Renamed".into();
        changed_old.title = "Old name".into();
        let removed = event("s1", "removed", t + Duration::hours(2), 30);
        let added = event("s1", "added", t + Duration::hours(3), 30);

        let cached = vec![unchanged.clone(), changed_old, removed.clone()];
        let fetched = vec![unchanged, changed_new.clone(), added.clone()];

        let diff = reconcile(&cached, &fetched);
        assert_eq!(diff.upserts, vec![changed_new, added], "unchanged is skipped");
        assert_eq!(diff.deletes, vec![event_key(&removed)]);
    }

    #[test]
    fn reconcile_treats_a_moved_occurrence_as_delete_plus_insert() {
        let t = local_instant(date("2026-08-12"), 9);
        let original = event("s1", "weekly", t, 30);
        let moved = event("s1", "weekly", t + Duration::hours(2), 30);

        let diff = reconcile(std::slice::from_ref(&original), std::slice::from_ref(&moved));
        assert_eq!(diff.upserts, vec![moved]);
        assert_eq!(diff.deletes, vec![event_key(&original)]);
    }

    #[test]
    fn reconcile_empty_fetch_deletes_everything() {
        let t = local_instant(date("2026-08-12"), 9);
        let cached = vec![event("s1", "a", t, 30), event("s1", "b", t + Duration::hours(1), 30)];
        let diff = reconcile(&cached, &[]);
        assert!(diff.upserts.is_empty());
        assert_eq!(diff.deletes.len(), 2);
    }

    // ── Materializer ────────────────────────────────────────────────────────

    fn enabled(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn materialize_creates_blocks_with_stable_ids_and_no_duplicates() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let ev = event("s1", "standup", local_instant(today, 9), 30);

        let first =
            materialize_window(&mut state, std::slice::from_ref(&ev), &enabled(&["s1"]), today);
        assert_eq!(first.upserted.len(), 1);
        assert_eq!(state.blocks.len(), 1);
        let block = &state.blocks[0];
        assert_eq!(block.id, external_block_id("s1", "standup", &ev.start));
        assert_eq!(block.todo_id, None);
        assert_eq!(
            block.source,
            BlockSource::External {
                uid: "standup".into(),
                subscription_id: Some("s1".into()),
                url: ev.url.clone(),
                busy: true,
                tentative: false,
            }
        );

        // Re-materializing is a no-op: same id, no sibling, nothing to persist.
        let second = materialize_window(&mut state, &[ev], &enabled(&["s1"]), today);
        assert_eq!(state.blocks.len(), 1);
        assert!(second.upserted.is_empty() && second.removed.is_empty());
    }

    #[test]
    fn materialize_respects_window_edges_in_local_days() {
        let today = date("2026-08-12");
        let (first_day, last_day) = window_days(today);
        let mut state = PlannerState::default();

        let events = vec![
            // 23:00 local on the last window day — the UTC/local trap: in any
            // timezone at or west of UTC-1 this lands on the next *UTC* day,
            // outside a naively-UTC window.
            event("s1", "late-edge", local_instant(last_day, 23), 30),
            event("s1", "first-day", local_instant(first_day, 9), 30),
            event("s1", "too-late", local_instant(last_day + Duration::days(1), 9), 30),
            event("s1", "too-early", local_instant(first_day - Duration::days(1), 9), 30),
        ];

        materialize_window(&mut state, &events, &enabled(&["s1"]), today);
        let mut uids: Vec<&str> = state
            .blocks
            .iter()
            .map(|b| match &b.source {
                BlockSource::External { uid, .. } => uid.as_str(),
                _ => panic!("only external blocks expected"),
            })
            .collect();
        uids.sort();
        assert_eq!(uids, vec!["first-day", "late-edge"]);
    }

    #[test]
    fn materialize_removes_vanished_and_disabled_but_spares_manual_blocks() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let keep = event("s1", "keep", local_instant(today, 9), 30);
        let vanish = event("s1", "vanish", local_instant(today, 11), 30);
        let other_sub = event("s2", "other", local_instant(today, 13), 30);

        materialize_window(
            &mut state,
            &[keep.clone(), vanish, other_sub.clone()],
            &enabled(&["s1", "s2"]),
            today,
        );
        assert_eq!(state.blocks.len(), 3);

        // Untouchables: a manual block, an auto block, and a legacy external
        // block from before subscriptions existed.
        for (id, source) in [
            ("manual", BlockSource::Manual),
            ("auto", BlockSource::Auto),
            (
                "legacy",
                BlockSource::External {
                    uid: "old".into(),
                    subscription_id: None,
                    url: None,
                    busy: true,
                    tentative: false,
                },
            ),
        ] {
            state.blocks.push(TimeBlock {
                id: id.into(),
                todo_id: None,
                title: id.into(),
                start: local_instant(today, 15),
                end: local_instant(today, 16),
                source,
            });
        }

        // Next pass: "vanish" left the feed, and s2 was disabled.
        let changes =
            materialize_window(&mut state, &[keep, other_sub], &enabled(&["s1"]), today);
        let removed: HashSet<&str> = changes.removed.iter().map(|b| {
            match &b.source {
                BlockSource::External { uid, .. } => uid.as_str(),
                _ => panic!("only external blocks may be removed"),
            }
        }).collect();
        assert_eq!(removed, HashSet::from(["vanish", "other"]));

        let surviving: HashSet<&str> = state.blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(surviving.contains("manual"));
        assert!(surviving.contains("auto"));
        assert!(surviving.contains("legacy"));
        assert_eq!(state.blocks.len(), 4, "keep + the three untouchables");
    }

    #[test]
    fn materialize_updates_a_block_whose_event_changed() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let mut ev = event("s1", "standup", local_instant(today, 9), 30);
        materialize_window(&mut state, std::slice::from_ref(&ev), &enabled(&["s1"]), today);

        // Same occurrence (same start), longer and renamed.
        ev.title = "Standup (extended)".into();
        ev.end = ev.start + Duration::minutes(45);
        let changes =
            materialize_window(&mut state, std::slice::from_ref(&ev), &enabled(&["s1"]), today);
        assert_eq!(changes.upserted.len(), 1);
        assert_eq!(state.blocks.len(), 1);
        assert_eq!(state.blocks[0].title, "Standup (extended)");
        assert_eq!(state.blocks[0].end, ev.end);
    }

    #[test]
    fn materialize_copies_the_busy_flag_from_the_event() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let mut free_ev = event("s1", "focus", local_instant(today, 9), 120);
        free_ev.busy = false;

        materialize_window(&mut state, &[free_ev], &enabled(&["s1"]), today);
        assert_eq!(state.blocks.len(), 1);
        match &state.blocks[0].source {
            BlockSource::External { busy, .. } => assert!(!busy, "free event → free block"),
            other => panic!("expected an external block, got {:?}", other),
        }
    }

    #[test]
    fn materialize_copies_the_tentative_flag_from_the_event() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let mut maybe_ev = event("s1", "maybe", local_instant(today, 9), 60);
        maybe_ev.tentative = true;

        materialize_window(&mut state, &[maybe_ev], &enabled(&["s1"]), today);
        assert_eq!(state.blocks.len(), 1);
        match &state.blocks[0].source {
            BlockSource::External { busy, tentative, .. } => {
                assert!(*tentative, "tentative event → tentative block");
                assert!(*busy, "tentative stays busy — it still blocks placement");
            }
            other => panic!("expected an external block, got {:?}", other),
        }
    }

    #[test]
    fn materialize_caches_but_never_materializes_all_day_events() {
        let today = date("2026-08-12");
        let mut state = PlannerState::default();
        let mut all_day = event("s1", "offsite", local_instant(today, 0), 24 * 60);
        all_day.all_day = true;

        let changes = materialize_window(&mut state, &[all_day], &enabled(&["s1"]), today);
        assert!(state.blocks.is_empty(), "a 24h block would wreck block_geometry");
        assert!(changes.upserted.is_empty());
    }

    #[test]
    fn window_bounds_open_and_close_on_local_midnights() {
        let today = date("2026-08-12");
        let (open, close) = window_bounds(today);
        let (first, last) = window_days(today);
        assert_eq!(open.with_timezone(&chrono::Local).date_naive(), first);
        // The close bound is the exclusive local midnight after the last day.
        assert_eq!(
            close.with_timezone(&chrono::Local).date_naive(),
            last + Duration::days(1)
        );
        assert_eq!((close - open).num_days(), 2 * WINDOW_DAYS + 1);
        // A late-evening event on the last local day is inside the bounds.
        let late = local_instant(last, 23);
        assert!(late >= open && late < close);
    }

    // ── Fetcher plumbing ────────────────────────────────────────────────────

    /// End-to-end-ish, no network and no store: ICS text → parse → reconcile
    /// → materialize produces the right external `TimeBlock`s.
    #[test]
    fn ics_body_flows_through_parse_reconcile_materialize() {
        let today = date("2026-08-12");
        let window = window_bounds(today);

        // A weekly meeting plus a one-off, in UTC.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
BEGIN:VEVENT\r\nUID:weekly@test\r\nDTSTART:20260805T140000Z\r\nDTEND:20260805T143000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Team sync\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:oneoff@test\r\nDTSTART:20260812T160000Z\r\nDTEND:20260812T170000Z\r\nSUMMARY:Dentist\r\nURL:https://cal.example/dentist\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
        let fetched = crate::todo::ics::parse_ics(ics, "s1", window).unwrap();
        // The weekly rule expands only inside the ±14 day window (4–5
        // Wednesdays depending on timezone) plus the one-off.
        assert!(fetched.len() >= 4, "expected several occurrences, got {}", fetched.len());
        assert!(fetched.iter().all(|e| e.start >= window.0 && e.start < window.1));

        // Reconcile against an empty cache: everything is an upsert.
        let diff = reconcile(&[], &fetched);
        assert_eq!(diff.upserts.len(), fetched.len());
        assert!(diff.deletes.is_empty());

        // Materialize into an empty planner.
        let mut state = PlannerState::default();
        let changes = materialize_window(&mut state, &fetched, &enabled(&["s1"]), today);
        assert_eq!(changes.upserted.len(), fetched.len());
        assert_eq!(state.blocks.len(), fetched.len());

        let dentist = state
            .blocks
            .iter()
            .find(|b| b.title == "Dentist")
            .expect("the one-off materialized");
        assert_eq!(dentist.todo_id, None);
        assert_eq!(dentist.end - dentist.start, Duration::hours(1));
        assert_eq!(
            dentist.source,
            BlockSource::External {
                uid: "oneoff@test".into(),
                subscription_id: Some("s1".into()),
                url: Some("https://cal.example/dentist".into()),
                busy: true,
                tentative: false,
            }
        );

        // Next fetch: the one-off vanished. Reconcile flags exactly it, and
        // re-materializing drops its block.
        let without_oneoff: Vec<CalendarEvent> = fetched
            .iter()
            .filter(|e| e.uid != "oneoff@test")
            .cloned()
            .collect();
        let diff = reconcile(&fetched, &without_oneoff);
        assert!(diff.upserts.is_empty());
        assert_eq!(diff.deletes.len(), 1);

        let changes = materialize_window(&mut state, &without_oneoff, &enabled(&["s1"]), today);
        assert_eq!(changes.removed.len(), 1);
        assert!(state.blocks.iter().all(|b| b.title != "Dentist"));
    }

    fn ics_sub(id: &str) -> CalendarSubscription {
        CalendarSubscription {
            id: id.into(),
            name: "Feed".into(),
            color: "#fff".into(),
            enabled: true,
            source: CalendarSource::Ics {},
        }
    }

    #[test]
    fn resolve_fetcher_ics_requires_a_keychain_url() {
        let ics = ics_sub("s1");
        let composio = CalendarSubscription {
            id: "s2".into(),
            name: "Google".into(),
            color: "#fff".into(),
            enabled: true,
            source: CalendarSource::Composio {
                profile_id: "p".into(),
                calendar_id: "primary".into(),
            },
        };

        // With a URL: a real ICS fetcher, webcal rewritten to https.
        match resolve_fetcher(&ics, Some("webcal://example.com/feed.ics"), None) {
            Ok(Fetcher::Ics { url }) => assert_eq!(url, "https://example.com/feed.ics"),
            other => panic!("expected an ICS fetcher, got {:?}", other),
        }

        // Without one (or blank): an honest error, not a pretend sync.
        assert!(resolve_fetcher(&ics, None, None).is_err());
        assert!(resolve_fetcher(&ics, Some("  "), None).is_err());

        // Composio with no resolvable client: an honest reconnect-worthy
        // error, not a pretend sync.
        let err = resolve_fetcher(&composio, None, None).unwrap_err();
        assert!(err.contains("reconnect"), "{}", err);
    }

    #[test]
    fn fake_fetcher_returns_its_canned_result() {
        let sub = ics_sub("s1");
        let today = date("2026-08-12");
        let window = window_bounds(today);
        let ev = event("s1", "a", local_instant(today, 9), 30);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let prev = SyncState::default();
        let ok = Fetcher::Fake(Ok(vec![ev.clone()]));
        assert_eq!(
            rt.block_on(ok.fetch(&sub, window, &prev)).unwrap(),
            FetchOutcome::Fetched {
                events: vec![ev],
                etag: None,
                last_modified: None,
            }
        );

        let err = Fetcher::Fake(Err("boom".into()));
        assert_eq!(
            rt.block_on(err.fetch(&sub, window, &prev)).unwrap_err(),
            "boom"
        );
    }

    #[test]
    fn sync_state_roundtrips_through_json() {
        let state = SyncState {
            last_synced_at: Some("2026-08-12T09:00:00Z".parse().unwrap()),
            last_error: None,
            etag: Some("\"abc123\"".into()),
            last_modified: Some("Wed, 12 Aug 2026 09:00:00 GMT".into()),
        };
        let raw = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<SyncState>(&raw).unwrap(), state);
        // Older/empty payloads (including Phase 1's, without the HTTP
        // validators) default cleanly.
        assert_eq!(serde_json::from_str::<SyncState>("{}").unwrap(), SyncState::default());
        let phase1 = r#"{"last_synced_at":"2026-08-12T09:00:00Z","last_error":null}"#;
        let loaded: SyncState = serde_json::from_str(phase1).unwrap();
        assert_eq!(loaded.etag, None);
        assert_eq!(loaded.last_modified, None);
        assert!(loaded.last_synced_at.is_some());
    }
}
