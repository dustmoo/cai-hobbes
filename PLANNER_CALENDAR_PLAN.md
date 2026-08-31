# Planner Calendar Subscriptions — Implementation Plan

Read-only calendar subscriptions for the planner: external calendars (ICS/webcal feeds
and Composio Google Calendar) are mirrored into the Today timeline as read-only time
blocks, colored per subscription, deep-linking back to the source event when possible.

This document is the working spec for the phased build. Each phase lands with
automated tests (`cargo test --release`) — no manual test gates.

## Decisions (settled)

1. **Meetings subtract from capacity.** A day with 4h of meetings does not have a full
   day of task capacity. `measure_capacity` learns about external blocks; an opt-out
   settings toggle (`planner_calendar_counts_against_capacity`, default `true`)
   preserves the old behavior.
2. **Read-only mirror only.** No write-back to any calendar. Blocks with
   `BlockSource::External` stay non-draggable / non-resizable / non-deletable in the UI
   (already enforced in `planner_view.rs`; Phase 4 verifies all edit paths).
3. **Recurrence is transport-local, never modeled.** The Composio/Google path requests
   `singleEvents=true` so occurrences arrive pre-expanded. The ICS path expands
   RRULE/EXDATE locally (via the `rrule` crate) but only within the materialization
   window. `PlannerState` and the store never see a recurrence rule.
4. **Color + deep link.** Every subscription has a color; external blocks tint by it.
   Events carry an optional URL (`htmlLink` from Google via Composio; the `URL`
   property from ICS when present). Clicking an external block opens the URL via the
   `open` crate. No reconstruction of Google event URLs from UIDs (undocumented, brittle).
5. **ICS first, Composio second**, behind one fetcher interface. The subscription
   registry, sync loop, storage, materialization, and UI are transport-agnostic.

## Architecture

```
Settings.planner_calendar_subscriptions: Vec<CalendarSubscription>
        │
        ▼
CalendarSync coroutine (use_coroutine, modeled on summarization_scheduler.rs)
  ├── tick every ~15 min + SyncNow message + immediate pass on launch
  ├── per-subscription fetcher:
  │     Ics      → reqwest GET (ETag/Last-Modified aware) → parse → expand RRULEs in window
  │     Composio → GOOGLECALENDAR_EVENTS_LIST (singleEvents=true) via find_composio_client()
  ├── normalize to CalendarEvent (UTC instants)
  ├── cache: todo_calendar_events table (uid+subscription_id keyed, seq-guarded)
  └── reconcile + materialize window (today ± 14 days) into PlannerState.blocks
        as TimeBlock { todo_id: None, source: External { uid, subscription_id, url } }
```

Sync state (per-subscription `last_synced_at`, `etag`, last error) lives in the
`meta` key-value table in `sessions.db` (keys `cal_sync_<subscription_id>`).

## Data model

### `CalendarSubscription` (settings.rs)

```rust
pub struct CalendarSubscription {
    pub id: String,            // uuid
    pub name: String,
    pub color: String,         // hex, e.g. "#4f9cf9"
    pub enabled: bool,
    pub source: CalendarSource,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CalendarSource {
    Ics { /* URL lives in keychain, not settings */ },
    Composio { profile_id: String, calendar_id: String },
}
```

- Follows the `ProviderInstance` / `ComposioProfile` list pattern with
  add/remove/rename helpers.
- ICS URLs are secrets (Google/Outlook "secret address" feeds embed tokens): stored in
  the keychain as `cal_url_<subscription_id>`, indexed via a `cal_keys_index` CSV meta
  key — mirror of the `llm_api_key_<id>` / `llm_connector_keys_index` pattern in
  `secret_types.rs`. Settings holds only the subscription id. All keychain access via
  `save_secret_to_keychain` (P-011).
- The dead `planner_calendar_profile: Option<String>` stub is kept for serde compat
  and migrated: if set on load, convert it into a disabled Composio subscription and
  clear it (in `SettingsManager::load` next to the other migrations).

### `BlockSource::External` extension (todo/model.rs)

```rust
External {
    uid: String,
    #[serde(default)] subscription_id: Option<String>,
    #[serde(default)] url: Option<String>,
}
```

Serde defaults keep previously serialized blocks loading. `subscription_id` drives the
color tint; `url` drives click-to-open.

### `CalendarEvent` cache (todo/store.rs)

Migration `(2, MIGRATION_V2)` appended to `MIGRATIONS`:

```sql
CREATE TABLE IF NOT EXISTS todo_calendar_events (
    uid             TEXT NOT NULL,
    subscription_id TEXT NOT NULL,
    starts_at       TEXT NOT NULL,   -- UTC RFC3339 (fmt_ts), lexicographic == chronological
    ends_at         TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    data            TEXT NOT NULL,   -- full serialized CalendarEvent (source of truth)
    PRIMARY KEY (subscription_id, uid, starts_at)  -- recurring: one row per occurrence
);
```

**`max_seq` MUST include `todo_calendar_events`** — the hardcoded table array in
`store.rs` seeds the seq counter at startup; omitting the table makes all its writes
silently no-op after a restart (documented hazard, `store.rs` + PLANNER_DESIGN §9).

`CalendarEvent` (serialized in `data`): `uid, subscription_id, title, start, end
(DateTime<Utc>), all_day: bool, url: Option<String>, location: Option<String>`.

## Invariants (all phases)

- Instants stored UTC; compare/render via `.with_timezone(&Local).date_naive()` only
  (the `blocks_on` trap).
- Never hold the session-store mutex across `.await`; never acquire it while holding
  an MCP lock (P-010).
- External blocks have `todo_id: None`, so `prune_blocks_for_todo` / todo-deletion
  cascades never touch them — the sync reconciler is their **only** GC. Reconciliation
  is by `(subscription_id, uid, occurrence-start)`: upsert changed, delete vanished,
  purge everything for a removed/disabled subscription.
- External blocks occupy the timeline and count as busy for `first_free_slot` /
  auto-placement, and (per decision 1) subtract from capacity.
- New AI tools must be added to `PlannerClient::list_tools`, `BUILTIN_TOOLS`, and
  `is_planner_tool` — two drift-guard tests in `builtin_tools.rs` enforce this.
- Timeline geometry uses inline `style:` attributes only (Tailwind purges computed
  class names in release builds).

## Phases

### Phase 1 — Subscription model, storage, sync skeleton, capacity

The structural bulk. No network parsing yet — the fetcher is a trait with a test fake.

- `CalendarSubscription` / `CalendarSource` in settings + CRUD helpers + stub
  migration + keychain key scheme in `secret_types.rs`.
- `BlockSource::External` field extension (serde-compat tested).
- Store migration v2 + `todo_calendar_events` + `max_seq` entry + save/load/delete
  helpers following the existing row shape.
- `src/todo/calendar_sync.rs`: `CalendarFetcher` trait
  (`async fn fetch(&self, sub) -> Result<Vec<CalendarEvent>, String>`), reconciler
  (pure function over cached vs fetched), materializer (cache → window →
  `PlannerState.blocks` upsert/remove), coroutine wiring in `main.rs` with tick +
  `SyncNow` + launch pass.
- `measure_capacity` subtracts external-block minutes on the measured day (clamped at
  0), gated by `planner_calendar_counts_against_capacity` (new setting, default true).
- Tests: store round-trip + migration, seq seeding includes new table, reconciler
  add/update/remove/disable cases, materializer window edges (late-evening local-day
  trap), capacity subtraction, `BlockSource` serde back-compat.

### Phase 2 — ICS fetcher

- Deps: `icalendar` (or `ical`), `rrule`, `chrono-tz`.
- Parse VEVENTs; expand RRULE/RDATE/EXDATE inside the materialization window only;
  handle VTIMEZONE via chrono-tz; all-day (`DATE`-valued DTSTART) events flagged
  `all_day`; capture `URL` property when present; `webcal://` → `https://` rewrite.
- HTTP with the long-lived-client pattern (`Client::builder().timeout(...)`),
  ETag/If-Modified-Since; 304 short-circuits.
- Tests: fixture `.ics` files (single event, weekly RRULE with EXDATE, all-day,
  timezone-crossing, URL property) + `wiremock` for HTTP/ETag behavior.

### Phase 3 — Composio Google Calendar fetcher

- Same `CalendarFetcher` trait via
  `ComposioClient::execute_tool("GOOGLECALENDAR_EVENTS_LIST", {singleEvents: true,
  timeMin/timeMax = window})`, client resolved through `find_composio_client()`
  (clone the Arc before any await).
- Map `htmlLink` → `CalendarEvent.url`. Auth errors surface as subscription sync
  errors; the existing `is_auth_error()` → reconnect lifecycle handles token refresh.
- Calendar picker: list calendars via the toolkit to populate
  `CalendarSource::Composio.calendar_id`.
- Tests: response-mapping tests over canned Composio JSON fixtures.

### Phase 4 — Settings UI + timeline rendering

- `SettingsTab::Planner` arm: subscription list (name, color swatch, enabled toggle,
  last-synced/error status), add via URL paste or "Connect Google Calendar" (drives
  the existing marketplace connect flow), rename, remove (purges cache + blocks +
  keychain entry), "Sync now" button (sends `SyncNow`).
- Timeline: tint external blocks by subscription color; click opens `url` via `open`
  crate (no conflict — drag/resize already suppressed); verify **all** edit paths
  (resize handles, delete, context menu) are suppressed for `External`, not just
  `onmousedown`; render all-day events as a thin banner above the timeline; clamp
  out-of-window events into view instead of `block_geometry` returning `None` and
  dropping them silently.
- Tests: pure-logic tests for geometry clamping, all-day partitioning, color
  resolution; UI wiring is compile-checked (no interaction tests).

### Phase 5 — AI integration

- `planner_today_context`: interleave blocks with priority — Hobbes' own timeboxes
  first, then meetings — so a meeting-heavy day can't crowd timeboxes out of the
  `CTX_MAX_BLOCKS = 10` cap; meetings get a compact one-line format.
- New tool `HOBBES_CALENDAR_LIST` (date-range event listing for the AI) registered
  in all three places (see invariants).
- `HOBBES_PLAN_DAY` / auto-place treat external blocks as busy (verify
  `first_free_slot` already does once blocks are materialized).
- Tests: context-cap interleaving, tool handler tests in the existing
  `(&mut PlannerState, &Value, today, persist)` convention with `persist=false`.

## Post-P5 refinements

1. **Busy/Free/Focus-Time semantics.** `CalendarEvent` and `BlockSource::External`
   carry a `busy: bool` (serde-default `true`, so pre-existing rows load as busy).
   Sources: ICS `TRANSP:TRANSPARENT` → free; Composio `transparency == "transparent"`
   or `eventType == "focusTime"` → free; both transports share one title heuristic
   (`model::is_focus_time_title`: trimmed, case-insensitive "focus" / "focus time" /
   starts-with "focus time") for Google's Focus Time ICS exports. Non-busy blocks
   still render (dashed left bar, fainter fill, reduced opacity — inline styles) but
   are excluded from the auto-placement busy set, from `overlap_warning` (create and
   move), and from `meeting_planned_minutes` (so they never count as planned time);
   the `planner_today` context labels them `(meeting, free)`.
2. **Auto-placement never places before now (today only).** The timeline's
   "Add to timeline" first-fit clamps its search start via
   `placement_search_start(workday_start, now_local_min, is_today)`:
   `max(workday_start, now snapped FORWARD to the quarter grid)` on today, the
   plain workday start on future days. Manual drags and explicit AI-specified
   `HOBBES_TIME_BLOCK` times are untouched (the tool has no auto-placement path —
   `start`/`end` are required).
3. **Overlapping blocks share the rail width (≤ 3 columns).**
   `assign_overlap_columns` clusters transitively-overlapping intervals (touching
   edges don't overlap, matching `TimeBlock::overlaps`) and greedily assigns
   columns within each cluster; the count caps at 3 and overflow shares column 2.
   Rendering composes an inline horizontal fragment (`overlap_column_style`) with
   the existing vertical geometry; single-block clusters keep the classic
   full-width look. Applies to meetings and task blocks alike; drag math is
   x-independent and the empty-slot click target is unchanged.
4. **Meetings add to planned time; capacity stays the full day.** The first cut
   subtracted busy meeting minutes from `capacity_minutes`, which hid meeting time
   from "planned" and double-claimed slots where a task block overlapped a meeting.
   The model is now additive: `planned = todo estimates (open + done) +
   meeting_planned_minutes`, where `meeting_planned_minutes(blocks, date)` is the
   union of firm busy external-meeting intervals on the local day, minus any
   overlap with task blocks (Manual/Auto — the task wins; its estimate already
   claims the slot), summed. Overlapping meetings merge before summing.
   `capacity_minutes` is never shrunk; `remaining = capacity − planned`, so "free"
   means the full day minus tasks *and* meetings. The
   `planner_calendar_counts_against_capacity` setting (name kept for serde compat)
   now gates whether meetings join planned — off restores pure todo-estimate
   accounting.
5. **Tentative meeting status.** `CalendarEvent` and `BlockSource::External` carry
   `tentative: bool` (serde-default `false`, so cached rows and old blocks load as
   firm). Sources: ICS `STATUS:TENTATIVE`; Composio/Google `status == "tentative"`
   (CONFIRMED/absent → firm; CANCELLED is dropped upstream as before); the
   materializer copies it onto the block source. Accounting: a meeting contributes
   planned time only when `busy && !tentative` — tentative busy meetings still
   join the auto-placement busy set and overlap warnings (standard free-busy
   convention: they hold the slot) but add nothing to planned. Rendering: a
   tentative busy block keeps the busy tint but dashes the left color bar, and its
   hover tooltip reads "(tentative) Title"; the `planner_today` context labels it
   `(meeting, tentative)`.

## Out of scope

- Calendar write-back (create/edit/RSVP).
- macOS EventKit (PLANNER_DESIGN §7 Phase 2 — revisit after this ships).
- Recurrence modeling in `PlannerState`.
- Notifications/alarms from calendar events (the timer subsystem is one-shot and
  session-bound; separate feature if wanted).
