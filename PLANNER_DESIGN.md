# Hobbes Planner — Design & Implementation Plan

A built-in to-do list and daily planner, in the spirit of **Things for Mac** (structure:
Inbox / Today / Upcoming / Areas / Projects) crossed with **Sunsama** (ritual: plan the
day, estimate each task, flow it onto a timeline against a real capacity).

Two first-class surfaces, one data model:

- **AI surface** — the assistant creates, updates, completes and *plans* todos through
  built-in tools, and always sees today's plan in its system context.
- **User surface** — a full-width Planner view with a list column and a day timeline.

Status: **planned, not implemented.** This document is the spec.

---

## 1. Why this shape

The feature is only interesting if the AI and the human are editing *the same list*.
That rules out a chat-only "todo tool" (invisible to the user) and a UI-only panel
(invisible to the AI). So the data model lives outside both, in SQLite, and the two
surfaces are thin views over it.

The Sunsama half is what makes it more than a checklist: every todo carries an
**estimate**, every day carries a **capacity**, and planning is the act of pulling todos
onto a dated timeline until the estimates fill the capacity. Overcommitment becomes
visible instead of discovered at 6pm.

---

## 2. Data model

New module `src/todo/` (sibling of `src/skills/`, which is the precedent for a
domain module with its own model + store + registry):

```
src/todo/
  mod.rs        — re-exports, PlannerState
  model.rs      — Todo, Project, Area, TimeBlock, DayPlan + pure logic
  store.rs      — SQLite persistence
  handlers.rs   — AI tool handlers, (ToolCallStatus, String) returning
  views.rs      — pure query/grouping logic (today, upcoming, logbook…)
```

### 2.1 Core types (`model.rs`)

```rust
pub struct Todo {
    pub id: String,                        // uuid v4
    pub title: String,
    pub notes: String,                     // markdown
    pub status: TodoStatus,                // Open | Completed | Cancelled
    pub bucket: TodoBucket,                // Inbox | Anytime | Someday
    pub project_id: Option<String>,
    pub area_id: Option<String>,

    // Things-style "when" vs "deadline" — deliberately separate.
    pub scheduled_for: Option<NaiveDate>,  // the day you intend to do it
    pub time_of_day: Option<TimeOfDay>,    // Morning | Afternoon | Evening
    pub deadline: Option<NaiveDate>,       // the day it is actually due

    // Sunsama-style planning.
    pub estimate_minutes: Option<u32>,
    pub actual_minutes: u32,

    pub tags: Vec<String>,
    pub checklist: Vec<ChecklistItem>,
    pub sort_order: f64,                   // fractional index (see 2.3)
    pub origin: TodoOrigin,                // User | Ai { session_id }
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub struct TimeBlock {
    pub id: String,
    pub todo_id: Option<String>,           // None = meeting / ad-hoc block
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub source: BlockSource,               // Manual | Auto | External { uid }
}

pub struct DayPlan {
    pub date: NaiveDate,
    pub capacity_minutes: u32,             // seeded from settings, overridable per day
    pub planned_at: Option<DateTime<Utc>>, // the morning ritual happened
    pub shutdown_at: Option<DateTime<Utc>>,// the evening ritual happened
    pub reflection: String,
}

pub struct Project { id, title, notes, area_id, status, deadline, sort_order, … }
pub struct Area    { id, title, sort_order }
```

`scheduled_for` and `deadline` being distinct fields is the single most important
modelling decision inherited from Things — collapsing them into one "due date" is what
makes most to-do apps nag.

### 2.2 Storage (`store.rs`)

Reuse the existing SQLite file and connection. `src/session_store.rs` already owns a
process-wide `OnceLock<Mutex<Connection>>` on `~/.config/com.hobbes.app/sessions.db`,
with WAL mode and a shared `meta` KV table that already holds non-session data (window
size, lifetime counters). A second connection on the same file would mean a second WAL
writer for no benefit.

**Required change**: expose `pub(crate) fn with_conn` (currently private,
`session_store.rs:167`) so `todo::store` can borrow the same guarded connection.

Tables — a hybrid of indexed columns and a JSON blob, mirroring how the `sessions` row
is already built. Everything we filter or sort on gets a real column; the soft fields
(notes, checklist, tags) ride in `data` so they can evolve without `ALTER TABLE`:

```sql
CREATE TABLE IF NOT EXISTS todos (
    id             TEXT PRIMARY KEY,
    title          TEXT NOT NULL,
    status         TEXT NOT NULL,
    bucket         TEXT NOT NULL,
    project_id     TEXT,
    area_id        TEXT,
    scheduled_for  TEXT,          -- 'YYYY-MM-DD', NULL = unscheduled
    deadline       TEXT,
    estimate_mins  INTEGER,
    sort_order     REAL NOT NULL,
    completed_at   TEXT,
    updated_at     TEXT NOT NULL,
    seq            INTEGER NOT NULL,
    data           TEXT NOT NULL  -- notes, checklist, tags, origin, time_of_day…
);
CREATE INDEX IF NOT EXISTS idx_todos_scheduled ON todos(scheduled_for, status);
CREATE INDEX IF NOT EXISTS idx_todos_project   ON todos(project_id, status);

CREATE TABLE IF NOT EXISTS todo_projects  (…, seq INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS todo_areas     (…, seq INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS todo_blocks    (id TEXT PRIMARY KEY, todo_id TEXT, start TEXT,
                                           end TEXT, seq INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS todo_day_plans (date TEXT PRIMARY KEY, seq INTEGER NOT NULL,
                                           data TEXT NOT NULL);
```

A `todo_tags` join table was specced for tag filtering but **not built**: the whole
planner is held in memory, so tag filtering happens in Rust and the table would be an
unused write-amplification path. If tag queries ever need to run in SQL, that becomes
migration v2 — which is exactly what the runner is for.

Three non-negotiables inherited from the session store:

1. **Every table carries `seq`** and every upsert uses the
   `WHERE excluded.seq >= todos.seq` guard, so an out-of-order async write can't clobber
   a newer row.
2. **`seed_seq_from_db` must be extended** to `MAX(seq)` across the new tables. If it
   isn't, the process starts with a low counter and *every write is silently rejected*.
   This is the highest-risk detail in the whole plan.
3. **Test support mirrors `session_store::test_support`** — an in-memory DB so tests
   never touch the real file.

**Schema versioning.** The session store has no migration runner; it relies on
`CREATE TABLE IF NOT EXISTS` plus Rust-side migration of a JSON blob. Todos will need
real column additions over time, so `todo::store` owns a small numbered migration
runner.

> **Implemented differently to this spec.** The original plan called for
> `PRAGMA user_version`. That pragma is a *single* value for the whole database file,
> and `sessions.db` is shared — claiming it for the planner would block sessions from
> ever getting a migration path of their own. P0 instead uses a
> `schema_migrations(domain TEXT PRIMARY KEY, version INTEGER)` table, so each domain
> versions independently and sessions can adopt the same runner later.

**Write strategy.** Unlike sessions, todos are small and few. Hydrate *all* of them at
startup into a `Signal<PlannerState>`, and write through on each mutation
(`spawn_blocking` → upsert single row). No lazy hydration, no dirty-diff pass, no
fingerprint cache. This is a deliberate simplification and it's safe because the row
count is bounded by human effort.

### 2.3 Ordering

`sort_order: f64` as a fractional index: to drop an item between neighbours `a` and `b`,
write `(a + b) / 2.0`. Reordering touches one row instead of renumbering the list, which
matters for drag-and-drop responsiveness. Renormalise the whole list when the gap between
neighbours falls under a small epsilon.

---

## 3. AI surface

### 3.1 A new virtual server

Register `hobbes-planner` alongside `hobbes-core` and `hobbes-meta`, following the
established pattern in `src/mcp/manager.rs`:

- `pub const HOBBES_PLANNER_SERVER: &str = "hobbes-planner";` next to line 86/92.
- Add it to `is_builtin_virtual_server()` (line 98) so the user can't unload it and
  strand the tools.
- New `src/mcp/planner_client.rs` — a zero-state marker struct exposing `list_tools()`,
  exactly like `core_client.rs`.
- `McpClientType::NativePlanner` variant + `is_healthy()` arm + a dispatch arm returning
  the "was not intercepted" bug error.
- A spawn block in the init fan-out next to line 1195 sending an `ActiveMcpClient` built
  from `McpServerConfig::native_stub(...)`.

Why a separate server rather than more tools on `hobbes-core`: it lets the whole feature
be advertised or withheld as a unit, and it keeps the AI's own introspection honest
about where the tools come from.

### 3.2 The tools

Six, all accepting **arrays** where it makes sense so a planning turn is one call rather
than eight:

| Tool | Purpose |
|---|---|
| `HOBBES_TODO_CREATE` | Create one or more todos (title, notes, when, deadline, estimate, project, tags, checklist). |
| `HOBBES_TODO_UPDATE` | Patch any mutable field on one or more todos, including completing, cancelling, or reopening. |
| `HOBBES_TODO_LIST` | Query a view (`today`, `inbox`, `upcoming`, `anytime`, `someday`, `logbook`) or filter by project/tag/date range/text. |
| `HOBBES_PLAN_DAY` | The Sunsama ritual: assign an ordered set of todos to a date with estimates, and return the capacity math (planned vs available, with an explicit overcommit warning). |
| `HOBBES_TIME_BLOCK` | Create, move, or delete a block on the day timeline. |
| `HOBBES_PROJECT_UPSERT` | Create or update projects and areas. |

`HOBBES_PLAN_DAY` is the tool that carries the product opinion. It shouldn't silently
accept 11 hours of work into a 6-hour day — it returns the arithmetic and says so, and
the assistant is expected to surface that to the user.

Tool responses are **compact text**, not JSON dumps: `[td_1a2b] ○ Draft the proposal —
45m, today, #writing`. Tool results are re-fed into the next prompt, so line-oriented
output keeps the turn cheap. This mirrors `ScheduledTimer::summary()`.

### 3.3 Dispatch — and an existing bug to fix first

Built-in tools are intercepted *before* MCP dispatch. There are **two** such sites:

- `src/components/stream_manager.rs` (~line 538-780) — the normal streaming path.
- `src/components/chat.rs:349-383` — the permission-approval / resume path.

The second only handles `HOBBES_PAGE_RESULT` and `HOBBES_UPDATE_SCRATCHPAD`. The timer
tools and `HOBBES_INVOKE_SKILL` fall through there and would hit the
"was not intercepted before MCP dispatch. This is a Hobbes bug" error on a resumed turn.

**Do not add a third divergent copy.** Extract a shared
`dispatch_builtin_tool(...) -> Option<(ToolCallStatus, String)>` used by both sites, add
the planner arms to it, and close the timer/skill gap in the same change. This is a
prerequisite task, not a nice-to-have.

Handlers live in `src/todo/handlers.rs` as free functions over `&mut PlannerState`,
returning `(ToolCallStatus, String)` — the convention established by
`SessionState::handle_set_timer` and friends.

### 3.4 Context injection — what makes it feel built-in

A tool the model has to remember to call is a tool it forgets. `PromptBuilder`'s
`build_system_context()` assembles a keyed JSON map (`src/context/prompt_builder/system_context.rs`),
so add a `planner_today` key next to `scratchpad` (line 352):

```json
{
  "date": "2026-08-12",
  "capacity_minutes": 360,
  "planned_minutes": 245,
  "todos": ["[td_1a2b] ○ Draft the proposal — 45m", "[td_9f3c] ✓ Standup — 15m"],
  "blocks": ["09:30–10:15 Draft the proposal", "11:00–11:30 Standup"],
  "overdue": ["[td_77aa] Renew the domain — was due 2026-08-09"]
}
```

Placement in the compression tiers (`context_compression.rs:55-60`): **Tier 2** —
protected above the conversation summary, but droppable before the model runs out of
room. Not Tier 1; the scratchpad earns that, a to-do list does not.

Hard caps: at most ~20 todos, ~10 blocks, ~5 overdue, truncated titles. Budget roughly
400-600 tokens. Gated by `settings.planner_inject_today_context`.

---

## 4. User surface

### 4.1 Placement: a full-width view, not a sidebar

The three existing panels (Settings, History, MCP) are ~300px left sidebars that sit
*beside* the chat column. A planner with a list *and* a day timeline needs roughly
900px, so it doesn't fit that mould.

Render the Planner **in place of `ChatWindow`** in the main column
(`src/main.rs:2097`), gated on a `show_planner` signal, keeping the tab bar above it.
This reuses the existing mutual-exclusion toggle pattern without introducing a second
desktop window and its state-sharing problems. A separate always-on-top planner window
is a reasonable later addition; it is out of scope for v1.

### 4.2 Layout

```
┌───────────┬───────────────────────────────────┬──────────────────────┐
│ Inbox   3 │  Today · Wed 12 Aug               │ 4h 05m / 6h  ▓▓▓▓░░  │
│ Today   7 │  ┌─ quick add ───────────────────┐│ ─────────────────────│
│ Upcoming  │  ○ Draft the proposal   45m  ⚑   ││ 09:00                │
│ Anytime   │  ○ Review PR #212       20m      ││ 09:30 ┌────────────┐ │
│ Someday   │  ✓ Standup              15m      ││       │ Draft the  │ │
│ Logbook   │                                  ││ 10:15 └────────────┘ │
│ ───────── │  This Evening                    ││ 11:00 ┌ Standup ───┐ │
│ ▾ Work    │  ○ Read the spec        30m      ││ 11:30 └────────────┘ │
│   Hobbes  │                                  ││ …                    │
└───────────┴───────────────────────────────────┴──────────────────────┘
   ~200px              flex                          ~320px (Today only)
```

- **Left rail** — the Things list vocabulary, plus an Areas ▸ Projects tree with counts.
- **Centre** — grouped list, quick-add at the top (Enter commits, keeps focus), inline
  title editing, checkbox to complete, chips for estimate / deadline / tags.
- **Right** — the Sunsama half, shown only on Today: a capacity bar and an hour-ruled
  timeline with positioned blocks. Unscheduled todos for today sit below it and can be
  dragged onto the ruler.

### 4.3 Interaction notes

- **Dragging** (reorder in list, drop onto timeline) should use the **mouse-event**
  approach already proven by the panel resizer (`main.rs:1985-2073`): `onmousedown`
  captures the origin, a `fixed inset-0 z-50` overlay captures `onmousemove`/`onmouseup`.
  HTML5 drag-and-drop is used in exactly one place in the repo (file attachments) and is
  less predictable in the WebView.
- **Tailwind has no safelist** and purges classes not present literally in `.rs` source.
  Timeline geometry must therefore go through inline `style:` attributes
  (`style: "top: {top}px; height: {h}px;"`), never computed class names.
- Reuse the semantic theme tokens from `assets/main.css` (`bg-app`, `bg-section`,
  `bg-card`, `text-fg`, `text-fg-muted`, `border-subtle`, `border-faint`) so light mode
  works for free. Run `npm run build:css` after adding classes.
- Component conventions: zero props, state pulled from context via `use_context`,
  sub-components in the same file with typed props — as in `mcp_marketplace.rs`.

### 4.4 Wiring the toggle

A new `ChatCommand::TogglePlanner` needs all five touchpoints (the same list any new
toggle needs):

1. `ChatCommand` enum — `src/components/chat_input.rs:28`
2. Handler arm — the command `use_effect` in `src/main.rs:1632` (set `show_planner`,
   clear the other three panels)
3. Ignore-list arm in `chat_input.rs:174` (globally handled, not consumed by ChatInput)
4. Chat-bar icon — `chat_input.rs:707` (`ChatBarIconButton`, Feather `FiCheckSquare`)
5. Native menu (`src/menu.rs` + `MenuAction` at `main.rs:295`) and hotkey
   (`src/hotkey.rs:101`, defaults in `src/settings.rs`)

### 4.5 Inline in chat

When the assistant calls a planner tool, the result should render as an interactive card
list rather than a JSON blob — checkboxes that actually complete the todo.

Implement as a `tool_name` branch inside `ToolCallDisplay`
(`src/components/tool_call_display.rs:19`), which already receives the call and its
response and can reach `Signal<PlannerState>` through context. This avoids adding a
`MessageContent` variant and the message-schema/serde churn that would come with it.

---

## 5. Settings

New fields on `Settings` (`src/settings.rs`), each `#[serde(default)]` so old files load:

| Field | Default | Meaning |
|---|---|---|
| `planner_enabled` | `true` | Master switch; when off, tools are withheld from the prompt and the view is hidden. |
| `planner_inject_today_context` | `true` | Include the `planner_today` block in system context. |
| `planner_workday_start` / `_end` | `"09:00"` / `"17:00"` | Timeline bounds. |
| `planner_daily_capacity_minutes` | `360` | Default day capacity (6h of focused work). |
| `planner_auto_rollover` | `true` | Unfinished todos scheduled for a past day roll to today. |
| `planner_calendar_profile` | `None` | Composio profile to read calendar events from. |

Withholding tools uses the existing precedent at `system_context.rs:461`
(`tools.retain(|t| t.name != "HOBBES_INVOKE_SKILL")`).

Add a `SettingsTab::Planner` for the settings panel.

---

## 6. Reminders and rituals

The 5s poll loop in `main.rs:1187` that fires `ScheduledTimer`s is the right place to
also check the planner. Extend it to fire on:

- a time block starting within the next minute → toast
- a deadline reaching today → toast, once per day
- auto-rollover at first launch after midnight

The daily rituals reuse the existing timer machinery rather than inventing new
scheduling. A `ScheduledTimer` with `mode: Prompt` at 9am ("Let's plan your day — here's
what's on deck") *is* the Sunsama morning ritual, and the evening shutdown is the same
mechanism pointed at `DayPlan::reflection`. Both must be opt-in; an app that starts a
conversation on its own is disruptive by default.

---

## 7. Calendar

Real calendar integration is what makes time-blocking honest — blocking 10:00-11:00 is
fiction if a meeting already owns it.

**Phase 1 — Composio (recommended first).** Google Calendar is already an available
toolkit; read today's events through the connected profile and materialise them as
`TimeBlock`s with `source: External { uid }`, rendered read-only in a muted style. No
new native dependency, works on Windows, and reuses the existing OAuth lifecycle.

**Phase 2 — macOS EventKit.** Better fidelity and works offline, but costs a native
dependency, a calendar entitlement, an added privacy prompt, and notarization surface.
Worth doing, not worth blocking v1 on.

Write-back (pushing Hobbes time blocks into the real calendar) is explicitly out of
scope until read-only has proven itself.

---

## 8. Delivery phases

| Phase | Scope | Shippable outcome |
|---|---|---|
| **P0** | `src/todo/` model + store + `user_version` migrations + `PlannerState` hydration + settings fields. Unit tests on store and views. | Nothing user-visible; foundations are testable. |
| **P1** | Extract the shared built-in dispatcher (**fixes the existing timer/skill resume bug**), add `hobbes-planner` + the six tools + `planner_today` context injection. | The AI can fully manage todos. Verifiable in chat with no UI. |
| **P2** | Planner view: left rail, list, quick-add, inline edit, complete. `TogglePlanner` wiring. | The feature is usable by hand. |
| **P3** | Today view, estimates, capacity bar, timeline, drag-to-schedule, auto-rollover. | The Sunsama day-planning loop closes. |
| **P4** | Interactive todo cards inline in chat. | The two surfaces feel like one product. |
| **P5** | Block-start / deadline toasts, opt-in morning & shutdown rituals. | The planner becomes proactive. |
| **P6** | Composio calendar overlay. | Time-blocking becomes honest. |

P0 and P1 together are the smallest genuinely useful increment — an AI-managed to-do
list persisted across sessions, with no UI work at all. P2 is where it earns the
"Things-like" comparison; P3 is where it earns the Sunsama one.

---

## 8b. UX hardening

Post-P3 manual testing surfaced coherence gaps between the views, hidden model
data, and misleading affordances. Their remediation plan lives in
`PLANNER_UX_PLAN.md` (batches U1–U4).

## 9. Risks

- **Silent write rejection.** Forgetting to extend `seed_seq_from_db` with the new
  tables makes every planner write no-op with no error. Cover it with a store test that
  writes, reopens, and reads back.
- **Prompt bloat.** Six tool schemas plus the today block is roughly 900-1200 tokens on
  *every* turn. Mitigated by `planner_enabled` / `planner_inject_today_context` and hard
  item caps — but measure it against a real session before shipping P1.
- **Divergent dispatch sites.** Already a live bug (§3.3). If the shared dispatcher isn't
  extracted first, the planner inherits the same class of failure.
- **Purged Tailwind classes.** Any computed class name silently vanishes in release
  builds (the dev CDN masks this). Timeline geometry must use inline styles.
- **Lock discipline.** The todo store shares the session-store mutex. Never hold it
  across an `.await`, and never acquire it while holding an MCP lock — P-010's ordering
  mandate extends here.
- **Scope creep.** Recurrence rules, natural-language date parsing ("next Tuesday"),
  and calendar write-back are each large enough to swallow the project. Recurrence is
  deliberately absent from the v1 model; the AI can express it by creating the next
  instance on completion.

---

## 10. Open questions

1. ~~**Per-session or global?**~~ **Decided: global.** One list, visible from every tab,
   rather than session-scoped like the scratchpad and timers. Consequence to honour in
   implementation: the AI can see and modify work captured in *other* conversations, so
   the tool descriptions must say so plainly, and `Todo::origin` records the originating
   session for provenance.
2. **Should the assistant auto-capture todos** from conversation ("I need to renew the
   domain") without being asked? Powerful and slightly creepy; suggest starting with
   explicit-only and revisiting after P4.
3. **Estimate units** — minutes throughout, or Sunsama-style coarse buckets
   (15m/30m/1h/2h)? Minutes in the model regardless; the question is only what the UI
   offers.
