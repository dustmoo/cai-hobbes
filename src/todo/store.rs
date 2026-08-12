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

use super::model::{Area, DayPlan, Project, TimeBlock, Todo};
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

/// Ordered migrations. Append only — never edit a shipped entry, since
/// databases in the wild have already recorded it as applied.
const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1)];

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
    ["todos", "todo_projects", "todo_areas", "todo_blocks", "todo_day_plans"]
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
                            deadline, estimate_mins, sort_order, completed_at, updated_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
             title = excluded.title, status = excluded.status, bucket = excluded.bucket,
             project_id = excluded.project_id, area_id = excluded.area_id,
             scheduled_for = excluded.scheduled_for, deadline = excluded.deadline,
             estimate_mins = excluded.estimate_mins, sort_order = excluded.sort_order,
             completed_at = excluded.completed_at, updated_at = excluded.updated_at,
             seq = excluded.seq, data = excluded.data
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

pub fn delete_todo(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todos", id))
}

pub fn delete_project(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_projects", id))
}

pub fn delete_area(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_areas", id))
}

pub fn delete_block(id: &str) -> Result<(), String> {
    with_conn(|conn| delete_by_id_conn(conn, "todo_blocks", id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::model::{BlockSource, TodoBucket, TodoStatus};
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
                created_at: now,
                updated_at: now,
            };
            let block = TimeBlock {
                id: "blk_1".into(),
                todo_id: None,
                title: "Standup".into(),
                start: now,
                end: now + chrono::Duration::minutes(15),
                source: BlockSource::External { uid: "cal-99".into() },
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
}
