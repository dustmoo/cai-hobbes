//! SQLite persistence for the planner.
//!
//! Shares `sessions.db` and its single process-wide connection
//! (`session_store::with_conn`) rather than opening a second one — a second
//! connection on the same file would mean a second WAL writer for no benefit.
//!
//! Storage follows the shape the `sessions` table already uses: the full record
//! is serialised into a `data` JSON column (the single source of truth on read),
//! and the fields we filter or sort on are denormalised into real columns
//! alongside it. That keeps SQL queries possible where they matter while letting
//! the soft fields evolve without an `ALTER TABLE`.
//!
//! Writes are per-mutation rather than batched: planner rows are small and few
//! (bounded by human effort), so there is no dirty-diff pass and no fingerprint
//! cache. Every row still carries a `seq` with the same upsert guard the session
//! store uses, so two rapid edits to one row can't land out of order.

use rusqlite::{params, Connection, OptionalExtension};

use crate::session_store::{next_seq, with_conn};

use super::model::{
    AgentTimeSession, Area, CalendarEvent, DayPlan, FocusSession, Project, Proposal,
    ResolvedDecision, TimeBlock, TimeGoal, Todo,
};
use super::PlannerState;

// ── Schema & migrations ─────────────────────────────────────────────────────

/// Domain key in `schema_migrations`.
///
/// The migration table is keyed by domain rather than using `PRAGMA
/// user_version`, which is a single value for the whole database file. Since
/// `sessions.db` is shared, claiming that one slot for the planner would block
/// sessions from ever getting a migration path of their own.
const DOMAIN: &str = "todo";

const MIGRATION_V1: &str = "
    CREATE TABLE IF NOT EXISTS todos (
        id            TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        status        TEXT NOT NULL,
        bucket        TEXT NOT NULL,
        project_id    TEXT,
        area_id       TEXT,
        scheduled_for TEXT,
        deadline      TEXT,
        estimate_mins INTEGER,
        sort_order    REAL NOT NULL,
        completed_at  TEXT,
        updated_at    TEXT NOT NULL,
        seq           INTEGER NOT NULL,
        data          TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todos_scheduled ON todos(scheduled_for, status);
    CREATE INDEX IF NOT EXISTS idx_todos_project   ON todos(project_id, status);

    CREATE TABLE IF NOT EXISTS todo_projects (
        id         TEXT PRIMARY KEY,
        title      TEXT NOT NULL,
        area_id    TEXT,
        status     TEXT NOT NULL,
        sort_order REAL NOT NULL,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS todo_areas (
        id         TEXT PRIMARY KEY,
        title      TEXT NOT NULL,
        sort_order REAL NOT NULL,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );

    -- 'end' is a SQLite keyword; the columns are starts_at/ends_at to avoid quoting.
    CREATE TABLE IF NOT EXISTS todo_blocks (
        id        TEXT PRIMARY KEY,
        todo_id   TEXT,
        starts_at TEXT NOT NULL,
        ends_at   TEXT NOT NULL,
        seq       INTEGER NOT NULL,
        data      TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todo_blocks_start ON todo_blocks(starts_at);

    CREATE TABLE IF NOT EXISTS todo_day_plans (
        date TEXT PRIMARY KEY,
        seq  INTEGER NOT NULL,
        data TEXT NOT NULL
    );
";

/// External calendar event cache. The full `CalendarEvent` is serialized into
/// `data`; the time columns are denormalized for windowed queries. Recurring
/// events land pre-expanded, one row per occurrence, hence `starts_at` in the
/// primary key.
const MIGRATION_V2: &str = "
    CREATE TABLE IF NOT EXISTS todo_calendar_events (
        uid             TEXT NOT NULL,
        subscription_id TEXT NOT NULL,
        starts_at       TEXT NOT NULL,
        ends_at         TEXT NOT NULL,
        seq             INTEGER NOT NULL,
        data            TEXT NOT NULL,
        PRIMARY KEY (subscription_id, uid, starts_at)
    );
    CREATE INDEX IF NOT EXISTS idx_todo_calendar_events_start ON todo_calendar_events(starts_at);
";

/// Focus-session history: one row per focus sitting, `person` vs `agent`
/// attribution. The full `FocusSession` serializes into `data`; the columns
/// we filter and aggregate on are denormalized. `ended_at` is NULL while the
/// session is live.
const MIGRATION_V3: &str = "
    CREATE TABLE IF NOT EXISTS todo_focus_sessions (
        id          TEXT PRIMARY KEY,
        todo_id     TEXT NOT NULL,
        actor       TEXT NOT NULL,
        agent_session_id TEXT,
        started_at  TEXT NOT NULL,
        ended_at    TEXT,
        minutes     INTEGER NOT NULL DEFAULT 0,
        end_reason  TEXT,
        seq         INTEGER NOT NULL,
        data        TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todo_focus_sessions_todo  ON todo_focus_sessions(todo_id);
    CREATE INDEX IF NOT EXISTS idx_todo_focus_sessions_start ON todo_focus_sessions(started_at);
";

/// Eisenhower priority, denormalised from `data` like the other sortable
/// fields. Existing rows keep NULL — the JSON has no `priority` key, which
/// deserialises to `None` ("not triaged"), so no backfill is needed.
const MIGRATION_V4: &str = "
    ALTER TABLE todos ADD COLUMN priority TEXT;
";

/// Symbiotic planner (SYMBIOTIC_PLANNER.md §4). The new `Todo` fields (lane,
/// kind, outcome, agent_state, decision) live in the `todos.data` JSON and
/// default on read, so todos need no ALTER. Everything here is
/// `CREATE … IF NOT EXISTS`, so re-running the batch is a no-op.
///
/// - `todo_time_goals`: your-time goals, one row per category.
/// - `todo_agent_time_sessions`: the agent-time log (intent, tool calls,
///   outputs, cost).
/// - `todo_proposals`: agent-time outputs and their accept/dismiss status.
/// - `todo_decisions`: answered agent decisions (judgment count, ground
///   covered).
/// - `todo_kv`: planner singletons — the agent-time grant, the trust ledger
///   and the review-style signals — one JSON document per key.
const MIGRATION_V5: &str = "
    CREATE TABLE IF NOT EXISTS todo_time_goals (
        id         TEXT PRIMARY KEY,
        category   TEXT NOT NULL,
        sort_order REAL NOT NULL DEFAULT 0,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS todo_agent_time_sessions (
        id         TEXT PRIMARY KEY,
        started_at TEXT NOT NULL,
        ended_at   TEXT,
        cost_usd   REAL NOT NULL DEFAULT 0,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todo_agent_time_sessions_start
        ON todo_agent_time_sessions(started_at);

    CREATE TABLE IF NOT EXISTS todo_proposals (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        kind       TEXT NOT NULL,
        status     TEXT NOT NULL,
        created_at TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todo_proposals_status ON todo_proposals(status, created_at);

    CREATE TABLE IF NOT EXISTS todo_decisions (
        id          TEXT PRIMARY KEY,
        todo_id     TEXT NOT NULL,
        resolved_at TEXT NOT NULL,
        seq         INTEGER NOT NULL,
        data        TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_todo_decisions_resolved ON todo_decisions(resolved_at);

    CREATE TABLE IF NOT EXISTS todo_kv (
        key  TEXT PRIMARY KEY,
        seq  INTEGER NOT NULL,
        data TEXT NOT NULL
    );
";

/// Ordered migrations. Append only — never edit a shipped entry, since
/// databases in the wild have already recorded it as applied.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, MIGRATION_V1),
    (2, MIGRATION_V2),
    (3, MIGRATION_V3),
    (4, MIGRATION_V4),
    (5, MIGRATION_V5),
];

/// `todo_kv` keys.
pub const KV_AGENT_TIME_GRANT: &str = "agent_time_grant";
pub const KV_TRUST_LEDGER: &str = "trust_ledger";
pub const KV_REVIEW_SIGNALS: &str = "review_signals";

/// Create the planner schema and apply any pending migrations.
///
/// Called from `session_store::create_schema`, so it runs both on real startup
/// and for every in-memory test database.
pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             domain  TEXT PRIMARY KEY,
             version INTEGER NOT NULL
         );",
    )?;

    let current: i32 = conn
        .query_row(
            "SELECT version FROM schema_migrations WHERE domain = ?1",
            params![DOMAIN],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);

    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (domain, version) VALUES (?1, ?2)
             ON CONFLICT(domain) DO UPDATE SET version = excluded.version",
            params![DOMAIN, version],
        )?;
        tx.commit()?;
        tracing::info!("Planner schema migrated to v{}", version);
    }
    Ok(())
}

/// High-water mark across every planner table.
///
/// Folded into `session_store::seed_seq_from_db`. Without this a fresh process
/// starts its seq counter below rows written by a longer-lived earlier process,
/// and the upsert guards below silently reject every update — writes that appear
/// to succeed and vanish on restart.
pub(crate) fn max_seq(conn: &Connection) -> i64 {
    [
        "todos",
        "todo_projects",
        "todo_areas",
        "todo_blocks",
        "todo_day_plans",
        "todo_calendar_events",
        "todo_focus_sessions",
        "todo_time_goals",
        "todo_agent_time_sessions",
        "todo_proposals",
        "todo_decisions",
        "todo_kv",
    ]
        .iter()
        .map(|table| {
            conn.query_row(
                &format!("SELECT COALESCE(MAX(seq), 0) FROM {}", table),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

// ── Timestamp formatting ────────────────────────────────────────────────────

/// RFC3339 in UTC with microsecond precision, matching the session store, so
/// lexicographic ordering on the column equals chronological ordering.
fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn fmt_date(d: &chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

// ── Writes ──────────────────────────────────────────────────────────────────

pub fn upsert_todo_conn(conn: &Connection, todo: &Todo, seq: i64) -> rusqlite::Result<()> {
    let data = serde_json::to_string(todo).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(e))
    })?;
    conn.execute(
        "INSERT INTO todos (id, title, status, bucket, project_id, area_id, scheduled_for,
                            deadline, estimate_mins, sort_order, completed_at, updated_at, seq, data,
                            priority)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
             title = excluded.title, status = excluded.status, bucket = excluded.bucket,
             project_id = excluded.project_id, area_id = excluded.area_id,
             scheduled_for = excluded.scheduled_for, deadline = excluded.deadline,
             estimate_mins = excluded.estimate_mins, sort_order = excluded.sort_order,
             completed_at = excluded.completed_at, updated_at = excluded.updated_at,
             seq = excluded.seq, data = excluded.data, priority = excluded.priority
         WHERE excluded.seq >= todos.seq",
        params![
            todo.id,
            todo.title,
            todo.status.as_str(),
            todo.bucket.as_str(),
            todo.project_id,
            todo.area_id,
            todo.scheduled_for.as_ref().map(fmt_date),
            todo.deadline.as_ref().map(fmt_date),
            todo.estimate_minutes,
            todo.sort_order,
            todo.completed_at.as_ref().map(fmt_ts),
            fmt_ts(&todo.updated_at),
            seq,
            data,
            todo.priority.map(|p| p.as_str()),
        ],
    )?;
    Ok(())
}

pub fn upsert_project_conn(conn: &Connection, project: &Project, seq: i64) -> rusqlite::Result<()> {
    let data = serde_json::to_string(project)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_projects (id, title, area_id, status, sort_order, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             title = excluded.title, area_id = excluded.area_id, status = excluded.status,
             sort_order = excluded.sort_order, seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_projects.seq",
        params![
            project.id,
            project.title,
            project.area_id,
            project.status.as_str(),
            project.sort_order,
            seq,
            data
        ],
    )?;
    Ok(())
}

pub fn upsert_area_conn(conn: &Connection, area: &Area, seq: i64) -> rusqlite::Result<()> {
    let data = serde_json::to_string(area)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_areas (id, title, sort_order, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             title = excluded.title, sort_order = excluded.sort_order,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_areas.seq",
        params![area.id, area.title, area.sort_order, seq, data],
    )?;
    Ok(())
}

pub fn upsert_block_conn(conn: &Connection, block: &TimeBlock, seq: i64) -> rusqlite::Result<()> {
    let data = serde_json::to_string(block)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_blocks (id, todo_id, starts_at, ends_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
             todo_id = excluded.todo_id, starts_at = excluded.starts_at,
             ends_at = excluded.ends_at, seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_blocks.seq",
        params![
            block.id,
            block.todo_id,
            fmt_ts(&block.start),
            fmt_ts(&block.end),
            seq,
            data
        ],
    )?;
    Ok(())
}

pub fn upsert_day_plan_conn(conn: &Connection, plan: &DayPlan, seq: i64) -> rusqlite::Result<()> {
    let data = serde_json::to_string(plan)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_day_plans (date, seq, data) VALUES (?1, ?2, ?3)
         ON CONFLICT(date) DO UPDATE SET seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_day_plans.seq",
        params![fmt_date(&plan.date), seq, data],
    )?;
    Ok(())
}

pub fn upsert_focus_session_conn(
    conn: &Connection,
    session: &FocusSession,
    seq: i64,
) -> rusqlite::Result<()> {
    let data = serde_json::to_string(session)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_focus_sessions (id, todo_id, actor, agent_session_id, started_at,
                                          ended_at, minutes, end_reason, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
             todo_id = excluded.todo_id, actor = excluded.actor,
             agent_session_id = excluded.agent_session_id,
             started_at = excluded.started_at, ended_at = excluded.ended_at,
             minutes = excluded.minutes, end_reason = excluded.end_reason,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_focus_sessions.seq",
        params![
            session.id,
            session.todo_id,
            session.actor.as_str(),
            session.actor.agent_session_id(),
            fmt_ts(&session.started_at),
            session.ended_at.as_ref().map(fmt_ts),
            session.minutes,
            session.end_reason.map(|r| r.as_str()),
            seq,
            data
        ],
    )?;
    Ok(())
}

pub fn upsert_calendar_event_conn(
    conn: &Connection,
    event: &CalendarEvent,
    seq: i64,
) -> rusqlite::Result<()> {
    let data = serde_json::to_string(event)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO todo_calendar_events (uid, subscription_id, starts_at, ends_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(subscription_id, uid, starts_at) DO UPDATE SET
             ends_at = excluded.ends_at, seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_calendar_events.seq",
        params![
            event.uid,
            event.subscription_id,
            fmt_ts(&event.start),
            fmt_ts(&event.end),
            seq,
            data
        ],
    )?;
    Ok(())
}

pub fn delete_calendar_event_conn(
    conn: &Connection,
    subscription_id: &str,
    uid: &str,
    start: &chrono::DateTime<chrono::Utc>,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM todo_calendar_events
         WHERE subscription_id = ?1 AND uid = ?2 AND starts_at = ?3",
        params![subscription_id, uid, fmt_ts(start)],
    )?;
    Ok(())
}

pub fn delete_calendar_events_for_subscription_conn(
    conn: &Connection,
    subscription_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM todo_calendar_events WHERE subscription_id = ?1",
        params![subscription_id],
    )?;
    Ok(())
}

/// Cached calendar events, optionally scoped to one subscription, ordered by
/// start time (lexicographic on the RFC3339 column == chronological).
pub fn load_calendar_events_conn(
    conn: &Connection,
    subscription_id: Option<&str>,
) -> rusqlite::Result<Vec<CalendarEvent>> {
    let mut out = Vec::new();
    let mut collect = |raw: String| match serde_json::from_str::<CalendarEvent>(&raw) {
        Ok(v) => out.push(v),
        Err(e) => tracing::error!("Skipping unreadable todo_calendar_events row: {}", e),
    };
    match subscription_id {
        Some(id) => {
            let mut stmt = conn.prepare(
                "SELECT data FROM todo_calendar_events WHERE subscription_id = ?1 ORDER BY starts_at",
            )?;
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            for row in rows {
                collect(row?);
            }
        }
        None => {
            let mut stmt =
                conn.prepare("SELECT data FROM todo_calendar_events ORDER BY starts_at")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for row in rows {
                collect(row?);
            }
        }
    }
    Ok(out)
}

// ── Symbiotic planner writes ────────────────────────────────────────────────

fn to_json<T: serde::Serialize>(v: &T) -> rusqlite::Result<String> {
    serde_json::to_string(v).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

pub fn upsert_time_goal_conn(conn: &Connection, goal: &TimeGoal, seq: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO todo_time_goals (id, category, sort_order, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             category = excluded.category, sort_order = excluded.sort_order,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_time_goals.seq",
        params![goal.id, goal.category, goal.sort_order, seq, to_json(goal)?],
    )?;
    Ok(())
}

pub fn upsert_agent_time_session_conn(
    conn: &Connection,
    session: &AgentTimeSession,
    seq: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO todo_agent_time_sessions (id, started_at, ended_at, cost_usd, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
             started_at = excluded.started_at, ended_at = excluded.ended_at,
             cost_usd = excluded.cost_usd, seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_agent_time_sessions.seq",
        params![
            session.id,
            fmt_ts(&session.started_at),
            session.ended_at.as_ref().map(fmt_ts),
            session.cost_usd,
            seq,
            to_json(session)?
        ],
    )?;
    Ok(())
}

pub fn upsert_proposal_conn(conn: &Connection, proposal: &Proposal, seq: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO todo_proposals (id, session_id, kind, status, created_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             session_id = excluded.session_id, kind = excluded.kind,
             status = excluded.status, created_at = excluded.created_at,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_proposals.seq",
        params![
            proposal.id,
            proposal.session_id,
            proposal.kind.as_str(),
            proposal.status.as_str(),
            fmt_ts(&proposal.created_at),
            seq,
            to_json(proposal)?
        ],
    )?;
    Ok(())
}

pub fn upsert_decision_conn(
    conn: &Connection,
    decision: &ResolvedDecision,
    seq: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO todo_decisions (id, todo_id, resolved_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
             todo_id = excluded.todo_id, resolved_at = excluded.resolved_at,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_decisions.seq",
        params![
            decision.id,
            decision.todo_id,
            fmt_ts(&decision.resolved_at),
            seq,
            to_json(decision)?
        ],
    )?;
    Ok(())
}

pub fn put_kv_conn<T: serde::Serialize>(
    conn: &Connection,
    key: &str,
    value: &T,
    seq: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO todo_kv (key, seq, data) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= todo_kv.seq",
        params![key, seq, to_json(value)?],
    )?;
    Ok(())
}

/// A `todo_kv` document, or `T::default()` when absent or unreadable (an
/// unreadable singleton is logged, never fatal — e.g. the grant falls back
/// to its off-by-default shape).
pub fn get_kv_conn<T: serde::de::DeserializeOwned + Default>(
    conn: &Connection,
    key: &str,
) -> rusqlite::Result<T> {
    let raw: Option<String> = conn
        .query_row("SELECT data FROM todo_kv WHERE key = ?1", params![key], |r| r.get(0))
        .optional()?;
    Ok(match raw {
        Some(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::error!("Unreadable todo_kv '{}': {}", key, e);
            T::default()
        }),
        None => T::default(),
    })
}

fn delete_by_id_conn(conn: &Connection, table: &str, id: &str) -> rusqlite::Result<()> {
    let column = if table == "todo_day_plans" { "date" } else { "id" };
    conn.execute(
        &format!("DELETE FROM {} WHERE {} = ?1", table, column),
        params![id],
    )?;
    Ok(())
}

// ── Reads ───────────────────────────────────────────────────────────────────

/// Decode a table's `data` column into records, skipping (and logging) any row
/// that fails to parse rather than losing the whole list to one bad entry.
fn load_data_column<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<Vec<T>> {
    let mut stmt = conn.prepare(&format!("SELECT data FROM {}", table))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;

    let mut out = Vec::new();
    for row in rows {
        let raw = row?;
        match serde_json::from_str::<T>(&raw) {
            Ok(v) => out.push(v),
            Err(e) => tracing::error!("Skipping unreadable {} row: {}", table, e),
        }
    }
    Ok(out)
}

pub fn load_all_conn(conn: &Connection) -> rusqlite::Result<PlannerState> {
    Ok(PlannerState {
        todos: load_data_column(conn, "todos")?,
        projects: load_data_column(conn, "todo_projects")?,
        areas: load_data_column(conn, "todo_areas")?,
        blocks: load_data_column(conn, "todo_blocks")?,
        day_plans: load_data_column(conn, "todo_day_plans")?,
        focus_sessions: load_data_column(conn, "todo_focus_sessions")?,
        dirty_focus_sessions: Vec::new(),
        time_goals: {
            let mut goals: Vec<TimeGoal> = load_data_column(conn, "todo_time_goals")?;
            goals.sort_by(|a, b| {
                a.sort_order
                    .partial_cmp(&b.sort_order)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            goals
        },
        agent_time_grant: get_kv_conn(conn, KV_AGENT_TIME_GRANT)?,
        agent_time_sessions: {
            let mut v: Vec<AgentTimeSession> = load_data_column(conn, "todo_agent_time_sessions")?;
            v.sort_by_key(|s| s.started_at);
            v
        },
        proposals: {
            let mut v: Vec<Proposal> = load_data_column(conn, "todo_proposals")?;
            v.sort_by_key(|p| p.created_at);
            v
        },
        trust_ledger: get_kv_conn(conn, KV_TRUST_LEDGER)?,
        review_signals: get_kv_conn(conn, KV_REVIEW_SIGNALS)?,
        decision_log: {
            let mut v: Vec<ResolvedDecision> = load_data_column(conn, "todo_decisions")?;
            v.sort_by_key(|d| d.resolved_at);
            v
        },
        agent_time_lease: None,
    })
}

// ── Public API (against the shared connection) ──────────────────────────────

/// Hydrate the whole planner. Called once at startup — the list is small enough
/// that lazy loading would be complexity without benefit.
pub fn load_all() -> PlannerState {
    with_conn(load_all_conn).unwrap_or_else(|e| {
        tracing::error!("Failed to load planner state: {e}");
        PlannerState::default()
    })
}

pub fn save_todo(todo: &Todo) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_todo_conn(conn, todo, seq))
}

pub fn save_project(project: &Project) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_project_conn(conn, project, seq))
}

pub fn save_area(area: &Area) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_area_conn(conn, area, seq))
}

pub fn save_block(block: &TimeBlock) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_block_conn(conn, block, seq))
}

pub fn save_day_plan(plan: &DayPlan) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_day_plan_conn(conn, plan, seq))
}

pub fn save_focus_session(session: &FocusSession) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_focus_session_conn(conn, session, seq))
}

pub fn save_time_goal(goal: &TimeGoal) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_time_goal_conn(conn, goal, seq))
}

pub fn delete_time_goal(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_time_goals", id))
}

pub fn save_agent_time_session(session: &AgentTimeSession) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_agent_time_session_conn(conn, session, seq))
}

pub fn save_proposal(proposal: &Proposal) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_proposal_conn(conn, proposal, seq))
}

pub fn save_resolved_decision(decision: &ResolvedDecision) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_decision_conn(conn, decision, seq))
}

#[allow(dead_code)] // consumer: the Agent time panel (wave 2)
pub fn save_agent_time_grant(grant: &super::model::AgentTimeGrant) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| put_kv_conn(conn, KV_AGENT_TIME_GRANT, grant, seq))
}

pub fn save_trust_ledger(ledger: &super::model::TrustLedger) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| put_kv_conn(conn, KV_TRUST_LEDGER, ledger, seq))
}

#[allow(dead_code)] // consumer: the weekly review (wave 2)
pub fn save_review_signals(signals: &super::model::ReviewSignals) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| put_kv_conn(conn, KV_REVIEW_SIGNALS, signals, seq))
}

/// Deleting a todo also deletes its time blocks. The cascade lives here, not
/// in each caller: a caller that forgets it leaves orphaned block rows that
/// resurrect on the calendar at next launch.
pub fn delete_todo(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_todo_conn(conn, id))
}

fn delete_todo_conn(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM todo_blocks WHERE todo_id = ?1", params![id])?;
    delete_by_id_conn(conn, "todos", id)
}

#[allow(dead_code)] // consumer is project management UI, not yet built
pub fn delete_project(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_projects", id))
}

#[allow(dead_code)] // consumer is project management UI, not yet built
pub fn delete_area(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_areas", id))
}

pub fn delete_block(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_blocks", id))
}

pub fn delete_focus_session_conn(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    delete_by_id_conn(conn, "todo_focus_sessions", id)
}

pub fn delete_focus_session(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_focus_session_conn(conn, id))
}

pub fn save_calendar_event(event: &CalendarEvent) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| upsert_calendar_event_conn(conn, event, seq))
}

pub fn load_calendar_events(subscription_id: Option<&str>) -> Result<Vec<CalendarEvent>, String> {
    with_conn(|conn| load_calendar_events_conn(conn, subscription_id))
}

pub fn delete_calendar_event(
    subscription_id: &str,
    uid: &str,
    start: &chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    with_conn(|conn| delete_calendar_event_conn(conn, subscription_id, uid, start))
}

/// Purge a removed/disabled subscription's whole cache in one statement.
#[allow(dead_code)] // consumer is the Phase 4 settings UI (remove subscription)
pub fn delete_calendar_events_for_subscription(subscription_id: &str) -> Result<(), String> {
    with_conn(|conn| delete_calendar_events_for_subscription_conn(conn, subscription_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{BlockSource, Priority, TodoBucket, TodoStatus};
    use chrono::{NaiveDate, Utc};

    /// Planner tables land in the shared schema, so the session store's
    /// in-memory test database already has them.
    fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
        crate::session_store::test_support::with_test_db(f)
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn sample() -> Todo {
        let mut t = Todo::new("Draft the proposal", 1024.0);
        t.id = "td_1".into();
        t.estimate_minutes = Some(45);
        t.scheduled_for = Some(date("2026-08-12"));
        t.deadline = Some(date("2026-08-14"));
        t.tags = vec!["writing".into()];
        t.notes = "Outline first.".into();
        t
    }

    #[test]
    fn todo_survives_a_write_and_read() {
        with_db(|conn| {
            let todo = sample();
            upsert_todo_conn(conn, &todo, 1).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.todos.len(), 1);
            // Round-tripping through `data` must preserve the soft fields the
            // indexed columns don't carry.
            assert_eq!(loaded.todos[0], todo);
        });
    }

    #[test]
    fn indexed_columns_mirror_the_record() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 1).unwrap();

            let (status, scheduled, estimate): (String, String, i64) = conn
                .query_row(
                    "SELECT status, scheduled_for, estimate_mins FROM todos WHERE id = 'td_1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();

            assert_eq!(status, "open");
            assert_eq!(scheduled, "2026-08-12");
            assert_eq!(estimate, 45);
        });
    }

    #[test]
    fn scheduled_dates_are_queryable_in_sql() {
        with_db(|conn| {
            let mut today = sample();
            today.scheduled_for = Some(date("2026-08-12"));
            let mut later = sample();
            later.id = "td_2".into();
            later.scheduled_for = Some(date("2026-09-01"));

            upsert_todo_conn(conn, &today, 1).unwrap();
            upsert_todo_conn(conn, &later, 2).unwrap();

            let due: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM todos WHERE scheduled_for <= '2026-08-12' AND status = 'open'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(due, 1);
        });
    }

    #[test]
    fn a_stale_write_cannot_clobber_a_newer_row() {
        with_db(|conn| {
            let mut newer = sample();
            newer.title = "current".into();
            upsert_todo_conn(conn, &newer, 10).unwrap();

            let mut stale = sample();
            stale.title = "stale".into();
            upsert_todo_conn(conn, &stale, 5).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.todos[0].title, "current");
        });
    }

    #[test]
    fn max_seq_spans_every_planner_table() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 3).unwrap();

            let block = TimeBlock {
                id: "blk_1".into(),
                todo_id: Some("td_1".into()),
                title: "Draft".into(),
                start: Utc::now(),
                end: Utc::now(),
                source: BlockSource::Manual,
            };
            upsert_block_conn(conn, &block, 42).unwrap();

            // Seeding from `todos` alone would restart below 42 and silently
            // reject every subsequent block write.
            assert_eq!(max_seq(conn), 42);
        });
    }

    #[test]
    fn empty_database_seeds_from_zero() {
        with_db(|conn| assert_eq!(max_seq(conn), 0));
    }

    /// The failure this guards is silent: if `seed_seq_from_db` misses the
    /// planner tables, a restarted process draws seqs below the existing rows
    /// and the upsert guard rejects every write — saves appear to succeed and
    /// vanish on the next launch.
    #[test]
    fn writes_still_land_after_a_restart() {
        with_db(|conn| {
            let mut todo = sample();
            todo.title = "written by an earlier process".into();
            upsert_todo_conn(conn, &todo, 9_000_000).unwrap();

            // A fresh process seeds its counter from the store's high-water mark.
            crate::session_store::test_support::seed_seq(conn);

            todo.title = "written after restart".into();
            upsert_todo_conn(conn, &todo, next_seq()).unwrap();

            assert_eq!(
                load_all_conn(conn).unwrap().todos[0].title,
                "written after restart"
            );
        });
    }

    #[test]
    fn completing_a_todo_persists_the_closed_state() {
        with_db(|conn| {
            let mut todo = sample();
            upsert_todo_conn(conn, &todo, 1).unwrap();

            todo.mark_completed(Utc::now());
            upsert_todo_conn(conn, &todo, 2).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.todos[0].status, TodoStatus::Completed);
            assert!(loaded.todos[0].completed_at.is_some());

            let completed_at: Option<String> = conn
                .query_row("SELECT completed_at FROM todos WHERE id = 'td_1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(completed_at.is_some());
        });
    }

    #[test]
    fn deleting_a_todo_cascades_to_its_blocks() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 1).unwrap();
            let mine = TimeBlock {
                id: "blk_mine".into(),
                todo_id: Some("td_1".into()),
                title: "Draft".into(),
                start: Utc::now(),
                end: Utc::now(),
                source: BlockSource::Manual,
            };
            let other = TimeBlock {
                id: "blk_other".into(),
                todo_id: None,
                title: "Standup".into(),
                start: Utc::now(),
                end: Utc::now(),
                source: BlockSource::Manual,
            };
            upsert_block_conn(conn, &mine, 2).unwrap();
            upsert_block_conn(conn, &other, 3).unwrap();

            delete_todo_conn(conn, "td_1").unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert!(loaded.todos.is_empty());
            assert_eq!(loaded.blocks.len(), 1, "unlinked block must survive");
            assert_eq!(loaded.blocks[0].id, "blk_other");
        });
    }

    #[test]
    fn deleting_removes_the_row() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 1).unwrap();
            delete_by_id_conn(conn, "todos", "td_1").unwrap();
            assert!(load_all_conn(conn).unwrap().todos.is_empty());
        });
    }

    #[test]
    fn every_record_type_round_trips() {
        with_db(|conn| {
            let now = Utc::now();
            let area = Area {
                id: "ar_1".into(),
                title: "Work".into(),
                sort_order: 0.0,
                created_at: now,
                updated_at: now,
            };
            let project = Project {
                id: "pr_1".into(),
                title: "Hobbes".into(),
                notes: String::new(),
                area_id: Some("ar_1".into()),
                status: TodoStatus::Open,
                deadline: None,
                sort_order: 0.0,
                path: None,
                created_at: now,
                updated_at: now,
            };
            let block = TimeBlock {
                id: "blk_1".into(),
                todo_id: None,
                title: "Standup".into(),
                start: now,
                end: now + chrono::Duration::minutes(15),
                source: BlockSource::External {
                    uid: "cal-99".into(),
                    subscription_id: Some("sub1".into()),
                    url: Some("https://cal.example/e/99".into()),
                    busy: true,
                    tentative: false,
                },
            };
            let plan = DayPlan::new(date("2026-08-12"), 360);

            upsert_area_conn(conn, &area, 1).unwrap();
            upsert_project_conn(conn, &project, 2).unwrap();
            upsert_block_conn(conn, &block, 3).unwrap();
            upsert_day_plan_conn(conn, &plan, 4).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.areas, vec![area]);
            assert_eq!(loaded.projects, vec![project]);
            assert_eq!(loaded.blocks, vec![block]);
            assert_eq!(loaded.day_plans, vec![plan]);
        });
    }

    #[test]
    fn day_plans_are_keyed_by_date_not_id() {
        with_db(|conn| {
            let mut plan = DayPlan::new(date("2026-08-12"), 360);
            upsert_day_plan_conn(conn, &plan, 1).unwrap();

            plan.capacity_minutes = 240;
            upsert_day_plan_conn(conn, &plan, 2).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.day_plans.len(), 1, "same date must update in place");
            assert_eq!(loaded.day_plans[0].capacity_minutes, 240);
        });
    }

    #[test]
    fn one_corrupt_row_does_not_lose_the_rest() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 1).unwrap();
            conn.execute(
                "INSERT INTO todos (id, title, status, bucket, sort_order, updated_at, seq, data)
                 VALUES ('td_bad', 'x', 'open', 'inbox', 0.0, '2026-08-12T00:00:00Z', 2, 'not json')",
                [],
            )
            .unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.todos.len(), 1);
            assert_eq!(loaded.todos[0].id, "td_1");
        });
    }

    #[test]
    fn migrations_are_idempotent_and_recorded() {
        with_db(|conn| {
            // with_test_db already ran create_schema once.
            let version: i32 = conn
                .query_row(
                    "SELECT version FROM schema_migrations WHERE domain = 'todo'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(version, MIGRATIONS.last().unwrap().0);

            upsert_todo_conn(conn, &sample(), 1).unwrap();
            create_schema(conn).unwrap();
            create_schema(conn).unwrap();

            // Re-running must not drop data or re-apply an applied migration.
            assert_eq!(load_all_conn(conn).unwrap().todos.len(), 1);
        });
    }

    // ── Calendar event cache ────────────────────────────────────────────────

    fn sample_event(uid: &str, sub: &str, start: &str) -> CalendarEvent {
        let start: chrono::DateTime<Utc> = start.parse().unwrap();
        CalendarEvent {
            uid: uid.into(),
            subscription_id: sub.into(),
            title: "Standup".into(),
            start,
            end: start + chrono::Duration::minutes(30),
            all_day: false,
            url: Some("https://cal.example/e/1".into()),
            location: Some("Room 4".into()),
            busy: true,
            tentative: false,
        }
    }

    #[test]
    fn calendar_event_survives_a_write_and_read() {
        with_db(|conn| {
            let event = sample_event("ev1", "sub1", "2026-08-12T16:00:00Z");
            upsert_calendar_event_conn(conn, &event, 1).unwrap();

            let loaded = load_calendar_events_conn(conn, None).unwrap();
            assert_eq!(loaded, vec![event]);

            // Denormalized columns mirror the record.
            let (uid, sub, starts): (String, String, String) = conn
                .query_row(
                    "SELECT uid, subscription_id, starts_at FROM todo_calendar_events",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(uid, "ev1");
            assert_eq!(sub, "sub1");
            assert_eq!(starts, "2026-08-12T16:00:00.000000Z");
        });
    }

    #[test]
    fn calendar_events_filter_by_subscription() {
        with_db(|conn| {
            upsert_calendar_event_conn(conn, &sample_event("a", "sub1", "2026-08-12T16:00:00Z"), 1)
                .unwrap();
            upsert_calendar_event_conn(conn, &sample_event("b", "sub2", "2026-08-12T09:00:00Z"), 2)
                .unwrap();
            // Recurring: same uid, different occurrence start — its own row.
            upsert_calendar_event_conn(conn, &sample_event("a", "sub1", "2026-08-13T16:00:00Z"), 3)
                .unwrap();

            assert_eq!(load_calendar_events_conn(conn, None).unwrap().len(), 3);
            let sub1 = load_calendar_events_conn(conn, Some("sub1")).unwrap();
            assert_eq!(sub1.len(), 2);
            assert!(sub1.iter().all(|e| e.subscription_id == "sub1"));
            // Ordered by start.
            assert!(sub1[0].start < sub1[1].start);
            assert!(load_calendar_events_conn(conn, Some("nope")).unwrap().is_empty());
        });
    }

    #[test]
    fn a_stale_calendar_write_cannot_clobber_a_newer_row() {
        with_db(|conn| {
            let mut newer = sample_event("ev1", "sub1", "2026-08-12T16:00:00Z");
            newer.title = "current".into();
            upsert_calendar_event_conn(conn, &newer, 10).unwrap();

            let mut stale = newer.clone();
            stale.title = "stale".into();
            upsert_calendar_event_conn(conn, &stale, 5).unwrap();

            let loaded = load_calendar_events_conn(conn, None).unwrap();
            assert_eq!(loaded[0].title, "current");

            // An equal-or-newer seq updates in place (no duplicate row).
            newer.title = "updated".into();
            upsert_calendar_event_conn(conn, &newer, 11).unwrap();
            let loaded = load_calendar_events_conn(conn, None).unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].title, "updated");
        });
    }

    #[test]
    fn calendar_event_deletes_by_key_and_by_subscription() {
        with_db(|conn| {
            let ev1 = sample_event("a", "sub1", "2026-08-12T16:00:00Z");
            let ev2 = sample_event("a", "sub1", "2026-08-13T16:00:00Z");
            let ev3 = sample_event("b", "sub2", "2026-08-12T09:00:00Z");
            for (e, seq) in [(&ev1, 1), (&ev2, 2), (&ev3, 3)] {
                upsert_calendar_event_conn(conn, e, seq).unwrap();
            }

            // Keyed delete removes one occurrence, not the whole series.
            delete_calendar_event_conn(conn, "sub1", "a", &ev1.start).unwrap();
            assert_eq!(load_calendar_events_conn(conn, Some("sub1")).unwrap(), vec![ev2]);

            delete_calendar_events_for_subscription_conn(conn, "sub1").unwrap();
            assert!(load_calendar_events_conn(conn, Some("sub1")).unwrap().is_empty());
            assert_eq!(load_calendar_events_conn(conn, None).unwrap().len(), 1);
        });
    }

    #[test]
    fn max_seq_covers_the_calendar_table() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 3).unwrap();
            upsert_calendar_event_conn(
                conn,
                &sample_event("ev1", "sub1", "2026-08-12T16:00:00Z"),
                77,
            )
            .unwrap();

            // Seeding without todo_calendar_events would restart below 77 and
            // silently reject every subsequent calendar write.
            assert_eq!(max_seq(conn), 77);
        });
    }

    #[test]
    fn migration_v2_applies_to_an_existing_v1_database() {
        // A database that already recorded v1 (simulating a pre-calendar
        // install) must gain the calendar table from the append-only list.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 domain  TEXT PRIMARY KEY,
                 version INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (domain, version) VALUES ('todo', 1)",
            [],
        )
        .unwrap();
        // Pre-existing data must survive the upgrade.
        insert_pre_v4_todo(&conn, &sample());

        create_schema(&conn).unwrap();

        let version: i32 = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE domain = 'todo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // A v1 database runs forward through every later migration.
        assert_eq!(version, MIGRATIONS.last().unwrap().0);
        assert_eq!(load_all_conn(&conn).unwrap().todos.len(), 1);
        upsert_calendar_event_conn(&conn, &sample_event("ev1", "s", "2026-08-12T16:00:00Z"), 2)
            .unwrap();
        assert_eq!(load_calendar_events_conn(&conn, None).unwrap().len(), 1);
    }

    // ── Focus sessions ──────────────────────────────────────────────────────

    fn sample_session(id: &str) -> FocusSession {
        use crate::todo::model::{FocusActor, FocusEndReason};
        let started: chrono::DateTime<Utc> = "2026-08-12T16:00:00Z".parse().unwrap();
        let mut s = FocusSession::open(
            "td_1",
            started,
            FocusActor::Agent {
                session_id: Some("sess-7".into()),
            },
        );
        s.id = id.into();
        s.close(started + chrono::Duration::minutes(25), FocusEndReason::Paused);
        s
    }

    #[test]
    fn focus_session_round_trips_and_mirrors_columns() {
        with_db(|conn| {
            let session = sample_session("fs_1");
            upsert_focus_session_conn(conn, &session, 1).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.focus_sessions, vec![session.clone()]);

            let (actor, chat, started, ended, minutes, reason): (
                String,
                String,
                String,
                Option<String>,
                i64,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT actor, agent_session_id, started_at, ended_at, minutes, end_reason
                     FROM todo_focus_sessions WHERE id = 'fs_1'",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(actor, "agent");
            assert_eq!(chat, "sess-7");
            assert_eq!(started, "2026-08-12T16:00:00.000000Z");
            assert_eq!(ended.as_deref(), Some("2026-08-12T16:25:00.000000Z"));
            assert_eq!(minutes, 25);
            assert_eq!(reason.as_deref(), Some("paused"));

            // A live person session mirrors NULLs where it should.
            let open = FocusSession::open(
                "td_2",
                "2026-08-12T17:00:00Z".parse().unwrap(),
                crate::todo::model::FocusActor::Person,
            );
            upsert_focus_session_conn(conn, &open, 2).unwrap();
            let (actor, chat, ended, reason): (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = conn
                .query_row(
                    "SELECT actor, agent_session_id, ended_at, end_reason
                     FROM todo_focus_sessions WHERE id = ?1",
                    params![open.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(actor, "person");
            assert_eq!(chat, None);
            assert_eq!(ended, None);
            assert_eq!(reason, None);
        });
    }

    #[test]
    fn delete_focus_session_conn_removes_the_row() {
        with_db(|conn| {
            let session = sample_session("fs_1");
            upsert_focus_session_conn(conn, &session, 1).unwrap();
            assert_eq!(load_all_conn(conn).unwrap().focus_sessions.len(), 1);

            delete_focus_session_conn(conn, "fs_1").unwrap();
            assert!(load_all_conn(conn).unwrap().focus_sessions.is_empty());

            // Deleting an id that was never there is a no-op, not an error.
            delete_focus_session_conn(conn, "fs_missing").unwrap();
        });
    }

    #[test]
    fn a_stale_focus_session_write_cannot_clobber_a_newer_row() {
        with_db(|conn| {
            let mut newer = sample_session("fs_1");
            newer.minutes = 40;
            upsert_focus_session_conn(conn, &newer, 10).unwrap();

            let mut stale = sample_session("fs_1");
            stale.minutes = 5;
            upsert_focus_session_conn(conn, &stale, 4).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.focus_sessions.len(), 1);
            assert_eq!(loaded.focus_sessions[0].minutes, 40);
        });
    }

    #[test]
    fn max_seq_covers_the_focus_session_table() {
        with_db(|conn| {
            upsert_todo_conn(conn, &sample(), 3).unwrap();
            upsert_focus_session_conn(conn, &sample_session("fs_1"), 91).unwrap();

            // Seeding without todo_focus_sessions would restart below 91 and
            // silently reject every subsequent session write.
            assert_eq!(max_seq(conn), 91);
        });
    }

    #[test]
    fn migration_v3_applies_to_an_existing_v2_database() {
        // A database that already recorded v2 (a pre-focus-session install)
        // must gain the session table from the append-only list.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 domain  TEXT PRIMARY KEY,
                 version INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (domain, version) VALUES ('todo', 2)",
            [],
        )
        .unwrap();
        insert_pre_v4_todo(&conn, &sample());

        create_schema(&conn).unwrap();

        let version: i32 = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE domain = 'todo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, MIGRATIONS.last().unwrap().0);
        assert_eq!(load_all_conn(&conn).unwrap().todos.len(), 1);
        upsert_focus_session_conn(&conn, &sample_session("fs_1"), 2).unwrap();
        assert_eq!(load_all_conn(&conn).unwrap().focus_sessions.len(), 1);
    }

    /// A todo row as a pre-v4 build wrote it: no `priority` column, and no
    /// `priority` key in the JSON.
    fn insert_pre_v4_todo(conn: &Connection, todo: &Todo) {
        let mut data = serde_json::to_value(todo).unwrap();
        data.as_object_mut().unwrap().remove("priority");
        conn.execute(
            "INSERT INTO todos (id, title, status, bucket, sort_order, updated_at, seq, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
            params![
                todo.id,
                todo.title,
                todo.status.as_str(),
                todo.bucket.as_str(),
                todo.sort_order,
                fmt_ts(&todo.updated_at),
                data.to_string(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn migration_v4_adds_priority_and_old_rows_load_unset() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 domain  TEXT PRIMARY KEY,
                 version INTEGER NOT NULL
             );",
        )
        .unwrap();
        for sql in [MIGRATION_V1, MIGRATION_V2, MIGRATION_V3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute(
            "INSERT INTO schema_migrations (domain, version) VALUES ('todo', 3)",
            [],
        )
        .unwrap();
        insert_pre_v4_todo(&conn, &sample());

        create_schema(&conn).unwrap();

        let loaded = load_all_conn(&conn).unwrap();
        assert_eq!(loaded.todos.len(), 1);
        assert_eq!(loaded.todos[0].priority, None, "old rows are not triaged");

        // ...and a fresh write fills the new column.
        let mut t = loaded.todos[0].clone();
        t.priority = Some(Priority::Important);
        upsert_todo_conn(&conn, &t, 2).unwrap();
        let col: Option<String> = conn
            .query_row("SELECT priority FROM todos WHERE id = ?1", params![t.id], |r| r.get(0))
            .unwrap();
        assert_eq!(col.as_deref(), Some("important"));
        assert_eq!(
            load_all_conn(&conn).unwrap().todos[0].priority,
            Some(Priority::Important)
        );
    }

    #[test]
    fn bucket_and_status_persist_across_reload() {
        with_db(|conn| {
            let mut todo = sample();
            todo.bucket = TodoBucket::Someday;
            todo.scheduled_for = None;
            upsert_todo_conn(conn, &todo, 1).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.todos[0].bucket, TodoBucket::Someday);
            assert!(loaded.todos[0].scheduled_for.is_none());
        });
    }

    // ── Symbiotic planner (v5) ──────────────────────────────────────────────

    /// A database at v4 (every pre-lanes migration applied, with a todo row
    /// written before the lane fields existed).
    fn v4_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 domain  TEXT PRIMARY KEY,
                 version INTEGER NOT NULL
             );",
        )
        .unwrap();
        for sql in [MIGRATION_V1, MIGRATION_V2, MIGRATION_V3, MIGRATION_V4] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute("INSERT INTO schema_migrations (domain, version) VALUES ('todo', 4)", [])
            .unwrap();
        let todo = sample();
        let mut data = serde_json::to_value(&todo).unwrap();
        for key in ["lane", "kind", "time_category", "outcome", "agent_state", "decision"] {
            data.as_object_mut().unwrap().remove(key);
        }
        conn.execute(
            "INSERT INTO todos (id, title, status, bucket, sort_order, updated_at, seq, data)
             VALUES (?1, ?2, 'open', 'inbox', 0, ?3, 1, ?4)",
            params![todo.id, todo.title, fmt_ts(&todo.updated_at), data.to_string()],
        )
        .unwrap();
        conn
    }

    #[test]
    fn migration_v5_upgrades_a_v4_db_and_old_rows_load_as_human() {
        use crate::todo::model::Lane;
        let conn = v4_db();
        create_schema(&conn).unwrap();
        let version: i32 = conn
            .query_row("SELECT version FROM schema_migrations WHERE domain = 'todo'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 5);
        let loaded = load_all_conn(&conn).unwrap();
        assert_eq!(loaded.todos.len(), 1);
        let t = &loaded.todos[0];
        assert_eq!(t.lane, Lane::Human, "pre-lanes rows are human to-dos");
        assert!(t.kind.is_none() && t.outcome.is_none() && t.agent_state.is_none() && t.decision.is_none());
        // Singletons default when absent: the grant is off.
        assert!(!loaded.agent_time_grant.enabled);
        assert_eq!(loaded.trust_ledger, Default::default());
        assert!(loaded.time_goals.is_empty() && loaded.proposals.is_empty());
    }

    #[test]
    fn migration_v5_is_idempotent() {
        let conn = v4_db();
        create_schema(&conn).unwrap();
        // Re-running the runner is a no-op, and so is replaying the v5 batch
        // itself (every statement is IF NOT EXISTS).
        create_schema(&conn).unwrap();
        conn.execute_batch(MIGRATION_V5).unwrap();
        // Even a version row knocked back (e.g. a downgrade then upgrade)
        // replays cleanly over the existing tables.
        conn.execute("UPDATE schema_migrations SET version = 4 WHERE domain = 'todo'", [])
            .unwrap();
        create_schema(&conn).unwrap();
        assert_eq!(load_all_conn(&conn).unwrap().todos.len(), 1);
    }

    #[test]
    fn symbiotic_rows_round_trip() {
        use crate::todo::model::*;
        with_db(|conn| {
            let now: chrono::DateTime<Utc> = "2026-10-07T17:00:00Z".parse().unwrap();
            let mut goal = TimeGoal::new("Outside", 240);
            goal.colour = Some("#8DBB7A".into());
            upsert_time_goal_conn(conn, &goal, 1).unwrap();

            let session = AgentTimeSession {
                id: "at_1".into(),
                intent: "Digest notes".into(),
                started_at: now,
                ended_at: Some(now + chrono::Duration::minutes(18)),
                cost_usd: 0.42,
                tool_calls: vec![AgentTimeLogEntry { at: now, tool: "notes.search".into(), summary: "6 notes".into(), kind: Default::default(), note: None }],
                outputs: vec!["pr_1".into()],
                end_reason: Some(AgentTimeEndReason::Completed),
                summary: Some("one digest".into()),
                chat_session_id: Some("chat".into()),
                out_of_scope_attempts: 0,
                todo_id: None,
            };
            upsert_agent_time_session_conn(conn, &session, 2).unwrap();
            let proposal = Proposal {
                id: "pr_1".into(),
                kind: ProposalKind::Research,
                title: "KV-cache digest".into(),
                body: "…".into(),
                status: ProposalStatus::Open,
                created_at: now,
                resolved_at: None,
                session_id: "at_1".into(),
                todo_id: None,
                lane: None,
            };
            upsert_proposal_conn(conn, &proposal, 3).unwrap();
            let decision = ResolvedDecision {
                id: "dc_1".into(),
                todo_id: "td_1".into(),
                question: "Ship four now?".into(),
                choice: "Ship four now".into(),
                note: None,
                worker: Some("puget".into()),
                asked_at: now,
                resolved_at: now,
                source: DecisionSource::Worker,
                delivery: Some(DecisionDelivery::NotDelivered { reason: "headless".into() }),
            };
            upsert_decision_conn(conn, &decision, 4).unwrap();
            let grant = AgentTimeGrant { enabled: true, daily_minutes: 45, spend_cap_usd: 2.0, ..Default::default() };
            put_kv_conn(conn, KV_AGENT_TIME_GRANT, &grant, 5).unwrap();
            let ledger = TrustLedger { proposals_accepted: 6, ..Default::default() };
            put_kv_conn(conn, KV_TRUST_LEDGER, &ledger, 6).unwrap();
            let mut signals = ReviewSignals::default();
            signals.record(ReviewStyle::Direct, NudgeOutcome::Dismissed);
            put_kv_conn(conn, KV_REVIEW_SIGNALS, &signals, 7).unwrap();
            // A stale singleton write never clobbers a newer one.
            put_kv_conn(conn, KV_AGENT_TIME_GRANT, &AgentTimeGrant::default(), 1).unwrap();

            let mut todo = sample();
            todo.set_lane(Lane::Agent);
            todo.outcome = Some("PR open".into());
            upsert_todo_conn(conn, &todo, 8).unwrap();
            let block = TimeBlock {
                id: "blk_yt".into(),
                todo_id: None,
                title: "Outside".into(),
                start: now,
                end: now + chrono::Duration::hours(1),
                source: BlockSource::YourTime { category: "Outside".into() },
            };
            upsert_block_conn(conn, &block, 9).unwrap();

            let loaded = load_all_conn(conn).unwrap();
            assert_eq!(loaded.time_goals, vec![goal]);
            assert_eq!(loaded.agent_time_sessions, vec![session]);
            assert_eq!(loaded.proposals, vec![proposal]);
            assert_eq!(loaded.decision_log, vec![decision]);
            assert_eq!(loaded.agent_time_grant, grant);
            assert_eq!(loaded.trust_ledger, ledger);
            assert_eq!(loaded.review_signals, signals);
            assert_eq!(loaded.todos[0], todo);
            assert_eq!(loaded.blocks[0], block);
            assert_eq!(max_seq(conn), 9, "max_seq spans the new tables");
        });
    }
}
