//! SQLite persistence for the fleet — its own migration domain in the shared
//! `sessions.db`, following `todo::store`'s shape: full record serialized
//! into a `data` JSON column, filter/sort fields denormalized alongside, and
//! every row carrying a `seq` under the same stale-write upsert guard.
//!
//! **max_seq rule**: both tables are folded into
//! [`max_seq`], which `session_store::seed_seq_from_db` consults — omitting a
//! table there means a restarted process draws seqs below existing rows and
//! every write silently vanishes on the next launch.

use rusqlite::{params, Connection, OptionalExtension};

use crate::session_store::{is_available, next_seq, with_conn};

use super::FleetSession;

/// Domain key in the shared `schema_migrations` table.
const DOMAIN: &str = "fleet";

const MIGRATION_V1: &str = "
    CREATE TABLE IF NOT EXISTS fleet_sessions (
        id            TEXT PRIMARY KEY,
        cwd           TEXT NOT NULL,
        name          TEXT NOT NULL,
        last_event_at TEXT NOT NULL,
        ended_at      TEXT,
        seq           INTEGER NOT NULL,
        data          TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_fleet_sessions_last_event ON fleet_sessions(last_event_at);

    CREATE TABLE IF NOT EXISTS fleet_events (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        event      TEXT NOT NULL,
        created_at TEXT NOT NULL,
        seq        INTEGER NOT NULL,
        data       TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_fleet_events_session ON fleet_events(session_id, created_at);
";

/// Ordered migrations. Append only — never edit a shipped entry.
const MIGRATIONS: &[(i32, &str)] = &[(1, MIGRATION_V1)];

/// Create the fleet schema / apply pending migrations. Called from
/// `session_store::create_schema`, so real startup and every in-memory test
/// database get these tables.
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
        tracing::info!("Fleet schema migrated to v{}", version);
    }
    Ok(())
}

/// High-water seq across the fleet tables — folded into
/// `session_store::seed_seq_from_db` (see module docs for why missing a
/// table here is silent data loss).
pub(crate) fn max_seq(conn: &Connection) -> i64 {
    ["fleet_sessions", "fleet_events"]
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

/// RFC3339 UTC with microsecond precision — lexicographic == chronological.
fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

// ── Sessions ────────────────────────────────────────────────────────────────

pub fn upsert_session_conn(
    conn: &Connection,
    session: &FleetSession,
    seq: i64,
) -> rusqlite::Result<()> {
    let data = serde_json::to_string(session)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO fleet_sessions (id, cwd, name, last_event_at, ended_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             cwd = excluded.cwd, name = excluded.name,
             last_event_at = excluded.last_event_at, ended_at = excluded.ended_at,
             seq = excluded.seq, data = excluded.data
         WHERE excluded.seq >= fleet_sessions.seq",
        params![
            session.id,
            session.cwd,
            session.name,
            fmt_ts(&session.last_event_at),
            session.ended_at.as_ref().map(fmt_ts),
            seq,
            data
        ],
    )?;
    Ok(())
}

/// One row by id, live or ended — the revival path when an event arrives for
/// a session that retired off the live map (its banked time must survive).
pub fn load_session_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<FleetSession>> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM fleet_sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
}

pub fn load_session(session_id: &str) -> Option<FleetSession> {
    if !is_available() {
        return None;
    }
    with_conn(|conn| load_session_conn(conn, session_id)).unwrap_or(None)
}

/// Live rows (`ended_at IS NULL`) for startup hydration.
pub fn load_live_conn(conn: &Connection) -> rusqlite::Result<Vec<FleetSession>> {
    let mut stmt = conn.prepare("SELECT data FROM fleet_sessions WHERE ended_at IS NULL")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        match serde_json::from_str::<FleetSession>(&row?) {
            Ok(s) => out.push(s),
            Err(e) => tracing::error!("Skipping unreadable fleet_sessions row: {}", e),
        }
    }
    Ok(out)
}

/// Banked minutes on local day `date` across **ended** sessions only — live
/// sessions are counted from the in-memory map, so filtering to ended rows
/// here is what keeps the two sources disjoint. Bounded by an SQL window on
/// `last_event_at` (a session that last spoke before the day began can't
/// have banked minutes on it).
pub fn ended_minutes_on_conn(
    conn: &Connection,
    date: chrono::NaiveDate,
) -> rusqlite::Result<u32> {
    // Widened by a day on each side to absorb the local-vs-UTC offset.
    let window_start = date
        .pred_opt()
        .unwrap_or(date)
        .format("%Y-%m-%d")
        .to_string();
    let mut stmt = conn.prepare(
        "SELECT data FROM fleet_sessions
         WHERE ended_at IS NOT NULL AND last_event_at >= ?1",
    )?;
    let rows = stmt.query_map(params![window_start], |r| r.get::<_, String>(0))?;
    let mut total: u32 = 0;
    for row in rows {
        if let Ok(s) = serde_json::from_str::<FleetSession>(&row?) {
            total = total.saturating_add(s.banked_seconds_on(date) / 60);
        }
    }
    Ok(total)
}

/// All rows — live AND ended — with activity on local day `date`: banked
/// minutes on it, or a last event / end falling on it (local). Bounded by the
/// same widened SQL window as [`ended_minutes_on_conn`]; the local-day test
/// happens in Rust. Feeds the Fleet "Today" section and day rollups.
pub fn sessions_active_on_conn(
    conn: &Connection,
    date: chrono::NaiveDate,
) -> rusqlite::Result<Vec<FleetSession>> {
    use chrono::Local;
    let window_start = date
        .pred_opt()
        .unwrap_or(date)
        .format("%Y-%m-%d")
        .to_string();
    let mut stmt = conn.prepare("SELECT data FROM fleet_sessions WHERE last_event_at >= ?1")?;
    let rows = stmt.query_map(params![window_start], |r| r.get::<_, String>(0))?;
    let on_date = |ts: &chrono::DateTime<chrono::Utc>| ts.with_timezone(&Local).date_naive() == date;
    let mut out = Vec::new();
    for row in rows {
        let Ok(s) = serde_json::from_str::<FleetSession>(&row?) else {
            continue;
        };
        let active = s.banked_seconds_on(date) > 0
            || on_date(&s.last_event_at)
            || s.ended_at.as_ref().is_some_and(&on_date);
        if active {
            out.push(s);
        }
    }
    Ok(out)
}

/// Seq-guard-safe read-modify-write for a row that may no longer be in the
/// live map (e.g. the final brief after `SessionEnd`). The whole RMW runs
/// inside one closure on the shared connection — atomic w.r.t. every other
/// writer — and rewrites with the caller's fresh seq, so the upsert guard
/// always accepts it. Returns false when the row is missing or unreadable.
pub fn merge_session_conn(
    conn: &Connection,
    session_id: &str,
    seq: i64,
    apply: impl FnOnce(&mut FleetSession),
) -> rusqlite::Result<bool> {
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM fleet_sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(data) = data else { return Ok(false) };
    let Ok(mut session) = serde_json::from_str::<FleetSession>(&data) else {
        tracing::error!("fleet: cannot merge into unreadable row {}", session_id);
        return Ok(false);
    };
    apply(&mut session);
    upsert_session_conn(conn, &session, seq)?;
    Ok(true)
}

/// Timestamp of the first fleet event on local day `date` — the clock-time
/// anchor for the exocortex multiplier. (Hobbes-tab activity doesn't append
/// events; on a terminal-quiet day this can be None even with agent minutes.)
pub fn first_event_on_conn(
    conn: &Connection,
    date: chrono::NaiveDate,
) -> rusqlite::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let (start, end) = crate::fleet::local_day_bounds_utc(date);
    let ts: Option<String> = conn
        .query_row(
            "SELECT MIN(created_at) FROM fleet_events WHERE created_at >= ?1 AND created_at < ?2",
            params![fmt_ts(&start), fmt_ts(&end)],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(ts.and_then(|t| t.parse().ok()))
}

pub fn first_event_on(date: chrono::NaiveDate) -> Option<chrono::DateTime<chrono::Utc>> {
    if !is_available() {
        return None;
    }
    with_conn(|conn| first_event_on_conn(conn, date)).unwrap_or(None)
}

// ── Events ──────────────────────────────────────────────────────────────────

/// One appended `fleet_events` row: hook events as they arrive, and gate
/// decisions (`event = "GateDecision"`) as they resolve.
pub fn append_event_conn(
    conn: &Connection,
    session_id: &str,
    event: &str,
    created_at: &chrono::DateTime<chrono::Utc>,
    data: &str,
    seq: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO fleet_events (id, session_id, event, created_at, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            uuid::Uuid::new_v4().to_string(),
            session_id,
            event,
            fmt_ts(created_at),
            seq,
            data
        ],
    )?;
    Ok(())
}

// ── Best-effort wrappers over the shared connection ─────────────────────────
//
// The listener persists through these. Store failure must never fail ingest —
// the in-memory state stays authoritative for the session either way (the
// same stance the planner takes on failed writes).

pub fn persist_sessions(rows: &[FleetSession]) {
    if !is_available() || rows.is_empty() {
        return;
    }
    for row in rows {
        let seq = next_seq();
        if let Err(e) = with_conn(|conn| upsert_session_conn(conn, row, seq)) {
            tracing::error!("fleet: failed to persist session {}: {}", row.id, e);
        }
    }
}

pub fn append_event(session_id: &str, event: &str, data: &str) {
    if !is_available() {
        return;
    }
    let seq = next_seq();
    let now = chrono::Utc::now();
    if let Err(e) = with_conn(|conn| append_event_conn(conn, session_id, event, &now, data, seq)) {
        tracing::error!("fleet: failed to append {} event: {}", event, e);
    }
}

pub fn load_live() -> Vec<FleetSession> {
    if !is_available() {
        return Vec::new();
    }
    with_conn(load_live_conn).unwrap_or_else(|e| {
        tracing::error!("fleet: failed to load live sessions: {}", e);
        Vec::new()
    })
}

pub fn ended_minutes_on(date: chrono::NaiveDate) -> u32 {
    if !is_available() {
        return 0;
    }
    with_conn(|conn| ended_minutes_on_conn(conn, date)).unwrap_or(0)
}

pub fn sessions_active_on(date: chrono::NaiveDate) -> Vec<FleetSession> {
    if !is_available() {
        return Vec::new();
    }
    with_conn(|conn| sessions_active_on_conn(conn, date)).unwrap_or_else(|e| {
        tracing::error!("fleet: failed to load day sessions: {}", e);
        Vec::new()
    })
}

pub fn merge_session(session_id: &str, apply: impl FnOnce(&mut FleetSession)) -> bool {
    if !is_available() {
        return false;
    }
    with_conn(|conn| merge_session_conn(conn, session_id, next_seq(), apply)).unwrap_or_else(|e| {
        tracing::error!("fleet: failed to merge session {}: {}", session_id, e);
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{FleetStatus, STALENESS_MINUTES};
    use chrono::{DateTime, NaiveDate, Utc};

    fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
        crate::session_store::test_support::with_test_db(f)
    }

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn sample(id: &str) -> FleetSession {
        let mut s = FleetSession::new(id, "/Users/x/dev/hobbes", utc("2026-08-25T10:00:00Z"));
        s.status = FleetStatus::Working;
        s.working_since = Some(utc("2026-08-25T10:00:00Z"));
        s.day_minutes.insert(date("2026-08-25"), 40);
        s
    }

    #[test]
    fn session_round_trips_and_mirrors_columns() {
        with_db(|conn| {
            let session = sample("cc-1");
            upsert_session_conn(conn, &session, 1).unwrap();

            let live = load_live_conn(conn).unwrap();
            assert_eq!(live, vec![session.clone()]);

            let (cwd, name, last, ended): (String, String, String, Option<String>) = conn
                .query_row(
                    "SELECT cwd, name, last_event_at, ended_at FROM fleet_sessions WHERE id = 'cc-1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(cwd, "/Users/x/dev/hobbes");
            assert_eq!(name, "hobbes");
            assert_eq!(last, "2026-08-25T10:00:00.000000Z");
            assert_eq!(ended, None);
        });
    }

    #[test]
    fn ended_sessions_leave_the_live_set_but_count_toward_day_minutes() {
        with_db(|conn| {
            let mut ended = sample("cc-1");
            ended.ended_at = Some(utc("2026-08-25T11:00:00Z"));
            ended.last_event_at = utc("2026-08-25T11:00:00Z");
            ended.status = FleetStatus::Idle;
            ended.working_since = None;
            upsert_session_conn(conn, &ended, 1).unwrap();
            upsert_session_conn(conn, &sample("cc-2"), 2).unwrap();

            let live = load_live_conn(conn).unwrap();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].id, "cc-2");

            // Only the ended row's banked minutes (live rows are the state's
            // job — the split keeps the sources disjoint).
            assert_eq!(ended_minutes_on_conn(conn, date("2026-08-25")).unwrap(), 40);
            assert_eq!(ended_minutes_on_conn(conn, date("2026-08-26")).unwrap(), 0);
        });
    }

    #[test]
    fn a_stale_session_write_cannot_clobber_a_newer_row() {
        with_db(|conn| {
            let mut newer = sample("cc-1");
            newer.name = "current".into();
            upsert_session_conn(conn, &newer, 10).unwrap();

            let mut stale = sample("cc-1");
            stale.name = "stale".into();
            upsert_session_conn(conn, &stale, 4).unwrap();

            assert_eq!(load_live_conn(conn).unwrap()[0].name, "current");
        });
    }

    #[test]
    fn events_append_and_keep_order() {
        with_db(|conn| {
            append_event_conn(conn, "cc-1", "SessionStart", &utc("2026-08-25T10:00:00Z"), "{}", 1)
                .unwrap();
            append_event_conn(conn, "cc-1", "Stop", &utc("2026-08-25T10:05:00Z"), "{}", 2).unwrap();
            append_event_conn(
                conn,
                "cc-1",
                "GateDecision",
                &utc("2026-08-25T10:06:00Z"),
                r#"{"request_id":"g1","outcome":"allow"}"#,
                3,
            )
            .unwrap();

            let events: Vec<String> = conn
                .prepare("SELECT event FROM fleet_events WHERE session_id = 'cc-1' ORDER BY created_at")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(events, vec!["SessionStart", "Stop", "GateDecision"]);
        });
    }

    #[test]
    fn max_seq_spans_both_fleet_tables() {
        with_db(|conn| {
            upsert_session_conn(conn, &sample("cc-1"), 7).unwrap();
            append_event_conn(conn, "cc-1", "Stop", &utc("2026-08-25T10:00:00Z"), "{}", 55)
                .unwrap();
            // Seeding without fleet_events would restart below 55 and silently
            // reject every subsequent fleet write.
            assert_eq!(max_seq(conn), 55);
            assert_eq!(
                crate::session_store::test_support::with_test_db(|c| max_seq(c)),
                0,
                "fresh database seeds from zero"
            );
        });
    }

    /// The cross-domain guard: `seed_seq_from_db` must see fleet rows, or a
    /// restarted process silently loses every fleet write.
    #[test]
    fn writes_still_land_after_a_restart() {
        with_db(|conn| {
            let mut session = sample("cc-1");
            upsert_session_conn(conn, &session, 9_500_000).unwrap();

            crate::session_store::test_support::seed_seq(conn);

            session.name = "after restart".into();
            upsert_session_conn(conn, &session, next_seq()).unwrap();
            assert_eq!(load_live_conn(conn).unwrap()[0].name, "after restart");
        });
    }

    #[test]
    fn migration_is_recorded_and_idempotent() {
        with_db(|conn| {
            let version: i32 = conn
                .query_row(
                    "SELECT version FROM schema_migrations WHERE domain = 'fleet'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(version, MIGRATIONS.last().unwrap().0);

            upsert_session_conn(conn, &sample("cc-1"), 1).unwrap();
            create_schema(conn).unwrap();
            create_schema(conn).unwrap();
            assert_eq!(load_live_conn(conn).unwrap().len(), 1);
        });
    }

    /// A pre-fleet database (todo domain recorded, no fleet domain) gains the
    /// fleet tables on the append-only path without touching existing data.
    #[test]
    fn fleet_domain_applies_to_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 domain  TEXT PRIMARY KEY,
                 version INTEGER NOT NULL
             );
             INSERT INTO schema_migrations (domain, version) VALUES ('todo', 3);",
        )
        .unwrap();

        create_schema(&conn).unwrap();

        let fleet_v: i32 = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE domain = 'fleet'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fleet_v, 1);
        // The todo domain's version is untouched by the fleet migration.
        let todo_v: i32 = conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE domain = 'todo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(todo_v, 3);
        upsert_session_conn(&conn, &sample("cc-1"), 1).unwrap();
        assert_eq!(load_live_conn(&conn).unwrap().len(), 1);
    }

    #[test]
    fn one_corrupt_row_does_not_lose_the_rest() {
        with_db(|conn| {
            upsert_session_conn(conn, &sample("cc-1"), 1).unwrap();
            conn.execute(
                "INSERT INTO fleet_sessions (id, cwd, name, last_event_at, seq, data)
                 VALUES ('bad', '/x', 'x', '2026-08-25T00:00:00Z', 2, 'not json')",
                [],
            )
            .unwrap();
            let live = load_live_conn(conn).unwrap();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].id, "cc-1");
        });
    }

    #[test]
    fn sessions_active_on_includes_live_and_ended_excludes_other_days() {
        with_db(|conn| {
            use chrono::{Local, TimeZone};
            let day = date("2026-08-25");
            let noon = Local
                .from_local_datetime(&day.and_hms_opt(12, 0, 0).unwrap())
                .earliest()
                .unwrap()
                .with_timezone(&Utc);

            // Live with banked minutes today.
            let mut live = sample("live");
            live.last_event_at = noon;
            upsert_session_conn(conn, &live, 1).unwrap();
            // Ended today, no banked minutes.
            let mut ended = FleetSession::new("ended", "/x/proj", noon);
            ended.ended_at = Some(noon);
            upsert_session_conn(conn, &ended, 2).unwrap();
            // Last spoke the day before (window catches it; filter drops it).
            let day_before = noon - chrono::Duration::hours(20);
            let mut old = FleetSession::new("old", "/x/old", day_before);
            old.ended_at = Some(day_before);
            upsert_session_conn(conn, &old, 3).unwrap();

            let mut ids: Vec<String> = sessions_active_on_conn(conn, day)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            ids.sort();
            assert_eq!(ids, vec!["ended", "live"]);
        });
    }

    #[test]
    fn merge_session_survives_the_seq_guard() {
        with_db(|conn| {
            let mut session = sample("cc-1");
            session.ended_at = Some(utc("2026-08-25T11:00:00Z"));
            upsert_session_conn(conn, &session, 9_000).unwrap();

            // Merge with a fresh higher seq: accepted despite the prior high seq.
            let merged = merge_session_conn(conn, "cc-1", 9_001, |s| {
                s.brief = Some(crate::fleet::SessionBrief {
                    headline: "final brief".into(),
                    bullets: vec![],
                    blocked_on: None,
                    generated_at: utc("2026-08-25T11:01:00Z"),
                    final_brief: true,
                });
                s.brief_dirty_at = None;
            })
            .unwrap();
            assert!(merged);
            let data: String = conn
                .query_row("SELECT data FROM fleet_sessions WHERE id = 'cc-1'", [], |r| r.get(0))
                .unwrap();
            let back: FleetSession = serde_json::from_str(&data).unwrap();
            assert_eq!(back.brief.unwrap().headline, "final brief");
            assert!(back.brief_dirty_at.is_none());
            assert!(back.ended_at.is_some(), "merge preserves the rest of the row");

            // Missing row → false; corrupt row → false, not an error.
            assert!(!merge_session_conn(conn, "nope", 9_002, |_| {}).unwrap());
            conn.execute(
                "INSERT INTO fleet_sessions (id, cwd, name, last_event_at, seq, data)
                 VALUES ('bad', '/x', 'x', '2026-08-25T00:00:00Z', 2, 'not json')",
                [],
            )
            .unwrap();
            assert!(!merge_session_conn(conn, "bad", 9_003, |_| {}).unwrap());
        });
    }

    #[test]
    fn revival_round_trip_keeps_banked_time() {
        with_db(|conn| {
            // A retired row with banked seconds…
            let mut s = sample("cc-1");
            s.day_seconds.insert(date("2026-08-25"), 300);
            s.ended_at = Some(utc("2026-08-25T11:00:00Z"));
            s.status = FleetStatus::Idle;
            s.working_since = None;
            upsert_session_conn(conn, &s, 5).unwrap();

            // …loads by id (the revival path), banked time intact.
            let back = load_session_conn(conn, "cc-1").unwrap().unwrap();
            assert_eq!(back.day_seconds.get(&date("2026-08-25")).copied(), Some(300));
            assert_eq!(back.day_minutes.get(&date("2026-08-25")).copied(), Some(40));
            assert!(back.ended_at.is_some());
            assert!(load_session_conn(conn, "nope").unwrap().is_none());
        });
    }

    #[test]
    fn staleness_constant_is_sane_for_the_cap_tests() {
        // The unclosed-span cap and the sweep share this window; a drive-by
        // change to one constant would silently skew banked minutes.
        assert_eq!(STALENESS_MINUTES, 10);
    }
}
