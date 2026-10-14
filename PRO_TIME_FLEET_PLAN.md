# Pro Time Analytics & Fleet Orchestration — Implementation Plan

Three deliverables, built in order:

- **Phase E — Entitlement**: a minimal honest Pro license mechanism (signed keys,
  keychain-stored, one accessor). Prerequisite for gating the rest.
- **Phase A — Focus-session time model**: person time vs agent time as first-class,
  non-destructive data; capacity lanes; the substrate for strategic-time analytics.
- **Phase B — Fleet observation & gates**: every Claude Code session on the machine
  (the user's iTerm windows) visible in Hobbes via global hooks, with attention
  routing and approval gates answered from the Hobbes UI.

Each phase lands with automated tests (`cargo test --release`, clippy clean); no
manual test gates. Nothing here is committed until the user says so.

## Background findings this plan rests on

- "Hobbes Pro" today is **packaging only**: `settings::is_sandboxed()` →
  `get_app_name()`. No license, no checks, no gates anywhere. (settings.rs:444/472,
  build_pro.sh.)
- Time tracking today is **one destructive counter**: `Todo.actual_minutes` +
  live `started_at`. `fold_elapsed` discards session history; no actor is recorded;
  AI-started focus (HOBBES_TODO_UPDATE → start_focus) is indistinguishable from the
  user's. `sanitize_stale_focus` clamps crash-recovered sessions to 120 min
  silently. The AI cannot stop focus except via status change.
- Claude Code **hooks are the fleet API**: hooks in `~/.claude/settings.json` run
  for every session machine-wide; `type: "http"` hooks POST event JSON to a URL and
  use the JSON response as the decision. `PermissionRequest` responses support
  allow/deny/escalate — an external app can answer gates. Events carry
  `session_id`, `cwd`, `transcript_path`. Prior art: disler/
  claude-code-hooks-multi-agent-observability (hooks→HTTP→SQLite→dashboard),
  agent-console (Rust, transcript discovery), octomux (permission inbox).

---

## Phase E — Entitlement (minimal, honest)

New module `src/entitlement.rs`:

- **License format**: `HOBBES-PRO.<base64url(payload_json)>.<base64url(sig)>` where
  `payload = { "email": ..., "issued_at": rfc3339, "product": "pro" }` and `sig` is
  an **ed25519** signature over the exact payload bytes. Public key embedded as a
  constant; private key never ships (a `scripts/mint_license/` helper — small Rust
  bin or cargo example — signs payloads with a key file the owner keeps offline,
  and can generate the keypair).
- **Storage**: keychain key `hobbes_license` via the existing SecretManager
  patterns (P-011); the parsed/verified state cached in memory after load.
- **API**: `Entitlement::verify(key_str) -> Result<LicenseInfo, LicenseError>` and
  a runtime accessor reachable where settings are (e.g. `pro_active()` alongside
  `get_app_name()` in settings.rs, reading a `Signal`/OnceLock set at startup and
  on key entry). Dev escape hatch: `debug_assertions` builds honor
  `HOBBES_PRO_DEV=1` env var; release builds never do.
- **UI**: Settings → About (next to the app name/attribution): license status
  (Free / Pro — licensed to {email}), paste-key input with verify-on-save +
  error display, remove-key button.
- **Gating rule for later phases**: features check `pro_active()` at their few
  entry points; recording/data collection is NOT gated (data is the user's), only
  the Pro *surfaces* (agent lane display, analytics readouts, fleet panel, gates).
  Free builds show a small "Pro" affordance where a gated surface would be.
- Tests: verify/reject (bad sig, tampered payload, wrong product, malformed),
  round-trip mint→verify in tests with an ephemeral keypair, dev-flag behavior
  gated to debug builds.

## Phase A — Focus-session time model

### Data

New table (migration v3 in `todo::store::MIGRATIONS`, **added to `max_seq`**):

```sql
CREATE TABLE IF NOT EXISTS todo_focus_sessions (
    id          TEXT PRIMARY KEY,
    todo_id     TEXT NOT NULL,
    actor       TEXT NOT NULL,            -- 'person' | 'agent'
    agent_session_id TEXT,                -- chat session that drove it, when actor='agent'
    started_at  TEXT NOT NULL,            -- fmt_ts UTC
    ended_at    TEXT,                     -- NULL while live
    minutes     INTEGER NOT NULL DEFAULT 0,
    end_reason  TEXT,                     -- stopped|completed|paused|preempted|cancelled|recovered
    seq         INTEGER NOT NULL,
    data        TEXT NOT NULL
);
```

Model type `FocusSession` (serialized in `data`, columns denormalized). Actor enum
`FocusActor { Person, Agent { session_id: Option<String> } }`.

### Behavior changes

- `start_focus(todo_id, now, actor)` — actor threaded from every call site: UI
  play/activate buttons → `Person`; `HOBBES_TODO_UPDATE` → `Agent { session_id }`
  (the dispatch layer knows the chat session id — same source as
  `TodoOrigin::Ai`). Opens a `FocusSession` row.
- Every banking path (`pause`, `mark_completed`, `reopen`, cancel, preemption in
  `start_focus`, `sanitize_stale_focus`) closes the open session row with the
  matching `end_reason` and its minutes. `actual_minutes` stays as the fast
  aggregate (back-compat for everything that reads it today); sessions are the
  source of truth for attribution.
- `sanitize_stale_focus` keeps the 120-min clamp for the *banked* aggregate but
  records the session honestly: `end_reason = 'recovered'`, real wall-clock
  bounds in the row, clamped minutes noted in `data`.
- AI stop path: `HOBBES_TODO_UPDATE` with `status: open` already pauses — document
  in the tool description that this is how the agent stops a timer.

### Surfaces

- **Capacity lanes**: person capacity math is unchanged BUT `done_minutes` for the
  day derives person-actor minutes from focus sessions (falling back to
  `actual_minutes` for todos with no session rows — pre-migration data). A new
  `agent_minutes` figure (agent-actor session minutes on the local day) rides on
  `Capacity`; it does NOT consume `capacity_minutes` and does not join
  `planned_minutes` — it renders as its own thin lane/segment in CapacityBar
  labeled distinctly, and as `"agent"` in `Capacity::summary()` when nonzero.
  **Pro-gated**: the agent lane and per-actor readouts; free behavior = today's.
- **FocusBar/tray**: when the live focus session's actor is Agent, show a small
  marker (e.g. "◇ agent" tag in FocusBar; tray tooltip gains "(agent)"). Timer
  behavior otherwise unchanged.
- **planner_today_context**: include per-actor done minutes so the AI can reason
  about person vs agent load (small JSON addition, capped).
- **Strategic-time readout (v1, Pro)**: in the Today rail header or day summary —
  for the current day: person focused total, agent total, and count/total of gaps
  between person sessions ≥ 5 min inside the workday window ("strategic time").
  Pure function over the day's sessions; no new charts yet.

### Tests

Session rows opened/closed by every path with right actor+reason (persist=false
convention for handlers); preemption closes as `preempted`; recovery rows honest;
per-actor day aggregation incl. cross-midnight sessions (UTC/local trap); capacity
lanes (agent minutes never consume capacity); back-compat: todos with no session
rows keep working; serde and migration round-trips; `max_seq` covers the table.

## Phase B — Fleet observation & approval gates (Pro)

### Ingest

- `src/fleet/` module: a minimal localhost HTTP listener (choose the lightest
  workable dep — tiny_http or hyper; no framework) on `127.0.0.1`, port chosen at
  startup (fixed default, fall back to ephemeral; persisted in meta so hook
  registration matches). Auth: a per-install shared token generated once, stored
  in meta, embedded in the hook URL path — events from anything without the token
  are rejected (localhost-only even so).
- **Hook registration UX**: Settings → a new "Fleet" section (Pro): "Connect
  Claude Code" button that MERGES hook entries into `~/.claude/settings.json`
  (parse-preserve-merge, timestamped backup first, idempotent re-run, and a
  "Disconnect" that removes exactly ours). Hooks registered (all `type: "http"`,
  pointing at `http://127.0.0.1:{port}/fleet/{token}/{event}`):
  `SessionStart`, `SessionEnd`, `Stop`, `Notification`, `PermissionRequest`.
  Modest timeouts except PermissionRequest (see Gates).
- Event store: `fleet_sessions` + `fleet_events` tables in sessions.db (own
  migration domain or the todo domain v4 — implementer's call, but **max_seq**
  rule applies). Live state also mirrored in a `Signal<FleetState>` for the UI.

### UI

- New left-rail section in the planner (or a Fleet tab — match existing nav
  idiom): list of known sessions — cwd-derived name, status (working / idle /
  **waiting on you**), last activity age, today's active minutes. Sessions expire
  from the live list after SessionEnd or staleness; history stays in the DB.
- Attention: sessions in `Notification`/`PermissionRequest` state sort to top with
  a clear "needs you" treatment; optionally bump the tray (reuse existing toast
  or tray tooltip — no new notification framework).

### Gates

- `PermissionRequest` handler: hold the HTTP response while the request is shown
  in the Fleet UI (tool name, input summary, session, cwd) with Approve / Deny
  buttons. Respect the hook timeout: respond `{"permissionDecision":"escalate"}`
  (or empty passthrough per hook contract — verify against docs at implementation
  time) shortly BEFORE the timeout if the user hasn't decided, so the terminal
  prompt appears as fallback — never silently swallow a gate. A per-session
  "auto-passthrough" toggle turns gating off for that session.
- Decisions logged to `fleet_events`.

### Time integration

- Fleet agent time (SessionStart→Stop/idle spans per external session) aggregates
  into the same per-day agent-minutes figure from Phase A (distinct source tag so
  in-app agent focus vs external fleet time remain separable in data).

### Explicitly out of scope (this round)

- iTerm2 window focusing/badging (Python API sidecar) — next round.
- Spawning/managed sessions from Hobbes (stream-json control protocol lane).
- OTel receiver; mobile/remote anything; multi-machine.

### Tests

Pure: event parsing (fixture JSONs per hook event), state reduction
(start/stop/notify/permission → session states), staleness expiry, settings.json
hook merge/unmerge idempotence (on temp files), gate timeout math, per-day fleet
minutes aggregation. HTTP: wiremock-style round-trips against the listener on an
ephemeral port (post fixture events, assert state + stored rows + held-response
behavior with a short test timeout).

## Invariants (all phases)

- UTC in storage, Local only at comparison/render (the `blocks_on` trap).
- P-010 lock ordering; never hold the store mutex across `.await`.
- New tables → `todo::store` (or session_store) migration + **`max_seq` array**.
- Keychain only via SecretManager (P-011).
- New AI tools (none planned this round) would need the 3-place registration.
- Recording is never Pro-gated; only surfaces are.
