//! SQLite-backed session persistence.
//!
//! Replaces the old whole-file `sessions.json` model. Each session is one row
//! whose `data` column holds the full serialized `Session`; lightweight
//! metadata columns (name, timestamps, cost, search text) let the history UI
//! list, search, and paginate without ever deserializing message blobs.
//!
//! Concurrency model: a single process-wide connection behind a `Mutex`.
//! Writes are guarded by a monotonic sequence number (`seq`) so async saves
//! that complete out of order can never overwrite a newer row with older data.
//! Dirty detection: `save_async` serializes each hydrated session on the
//! calling thread, hashes the bytes, and skips rows whose hash matches the
//! last successful write — so a save after a single-message append writes one
//! row, not the whole history.

use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::session::Session;

static CONN: OnceLock<Mutex<Connection>> = OnceLock::new();
static FINGERPRINTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
static SEQ: AtomicI64 = AtomicI64::new(1);

pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_ACTIVE_SESSION: &str = "active_session_id";
pub const META_WINDOW_WIDTH: &str = "window_width";
pub const META_WINDOW_HEIGHT: &str = "window_height";
pub const META_LIFETIME_COST: &str = "lifetime_cost";
pub const META_LIFETIME_TOKENS: &str = "lifetime_tokens";
pub const META_TOOL_CALL_HISTORY: &str = "tool_call_history";
const META_JSON_MIGRATED: &str = "migrated_from_json";

fn fingerprints() -> &'static Mutex<HashMap<String, u64>> {
    FINGERPRINTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn hash_bytes(data: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

pub fn get_db_path() -> Option<PathBuf> {
    dirs::config_dir().and_then(|mut path| {
        path.push("com.hobbes.app");
        std::fs::create_dir_all(&path).ok()?;
        path.push("sessions.db");
        Some(path)
    })
}

fn get_legacy_json_path() -> Option<PathBuf> {
    get_db_path().map(|p| p.with_file_name("sessions.json"))
}

fn get_legacy_archive_path() -> Option<PathBuf> {
    get_db_path().map(|p| p.with_file_name("sessions-archive.jsonl"))
}

pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS sessions (
             id            TEXT PRIMARY KEY,
             name          TEXT NOT NULL,
             last_updated  TEXT NOT NULL,
             message_count INTEGER NOT NULL,
             total_cost    REAL NOT NULL,
             total_tokens  INTEGER NOT NULL,
             has_timers    INTEGER NOT NULL,
             summary       TEXT NOT NULL,
             search_text   TEXT NOT NULL,
             seq           INTEGER NOT NULL,
             data          TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_sessions_last_updated ON sessions(last_updated DESC);
         CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL,
             seq   INTEGER NOT NULL
         );",
    )
}

/// Open the global connection, create the schema, and run the one-time
/// import from `sessions.json` + `sessions-archive.jsonl`. Idempotent;
/// safe to call from both `main()` and `app()`.
pub fn init() -> Result<(), String> {
    if CONN.get().is_some() {
        return Ok(());
    }
    let path = get_db_path().ok_or("could not resolve sessions.db path")?;
    let conn = Connection::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;

    // Restrict to owner-only, matching the old sessions.json (0600). SQLite
    // copies the db file's mode when it creates the -wal/-shm companions, so
    // setting it here covers those too; chmod any that already exist.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            let p = std::path::PathBuf::from(p);
            if p.exists() {
                if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)) {
                    tracing::warn!("Failed to set permissions on {}: {e}", p.display());
                }
            }
        }
    }

    create_schema(&conn).map_err(|e| format!("schema: {e}"))?;

    if let Err(e) = migrate_from_json_files(&conn) {
        // Import failure must not brick the app, but log loudly — the JSON
        // files are left untouched so the import retries next launch.
        tracing::error!("One-time JSON→SQLite import failed (will retry next launch): {e}");
    }

    let _ = CONN.set(Mutex::new(conn));
    Ok(())
}

pub fn is_available() -> bool {
    CONN.get().is_some()
}

fn with_conn<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
    let conn = CONN.get().ok_or("session store not initialized")?;
    let guard = conn.lock().map_err(|_| "session store lock poisoned")?;
    f(&guard).map_err(|e| e.to_string())
}

// ── One-time migration ──────────────────────────────────────────────────────

fn migrate_from_json_files(conn: &Connection) -> Result<(), String> {
    import_json_files(conn, get_legacy_json_path(), get_legacy_archive_path()).map(|_| ())
}

/// Path-parameterized import (testable). Returns (live, archive) import counts.
fn import_json_files(
    conn: &Connection,
    json_path: Option<PathBuf>,
    archive_path: Option<PathBuf>,
) -> Result<(usize, usize), String> {
    let already: Option<String> = meta_get_conn(conn, META_JSON_MIGRATED).map_err(|e| e.to_string())?;
    if already.is_some() {
        return Ok((0, 0));
    }

    let mut imported_live = 0usize;
    let mut imported_archive = 0usize;

    // 1. Live sessions.json — parse through the existing load/migration logic.
    if let Some(json_path) = json_path {
        if json_path.exists() {
            tracing::info!("Importing legacy sessions.json into sessions.db…");
            let state = crate::session::SessionState::load_from_json_file(&json_path)
                .map_err(|e| format!("parse sessions.json: {e}"))?;
            let tx_result: rusqlite::Result<()> = (|| {
                for session in state.sessions.values() {
                    let row = build_row(session, next_seq())
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    upsert_row(conn, &row)?;
                }
                meta_set_conn(conn, META_SCHEMA_VERSION, &state.schema_version.to_string())?;
                meta_set_conn(conn, META_ACTIVE_SESSION, &state.active_session_id)?;
                meta_set_conn(conn, META_WINDOW_WIDTH, &state.window_width.to_string())?;
                meta_set_conn(conn, META_WINDOW_HEIGHT, &state.window_height.to_string())?;
                meta_set_conn(conn, META_LIFETIME_COST, &state.lifetime_cost.to_string())?;
                meta_set_conn(conn, META_LIFETIME_TOKENS, &state.lifetime_tokens.to_string())?;
                let tch = serde_json::to_string(&state.tool_call_history).unwrap_or_else(|_| "[]".into());
                meta_set_conn(conn, META_TOOL_CALL_HISTORY, &tch)?;
                Ok(())
            })();
            tx_result.map_err(|e| format!("import sessions.json rows: {e}"))?;
            imported_live = state.sessions.len();
        }
    }

    // 2. Archive JSONL — sessions GC'd or recovered from old backups.
    //    Live sessions win on id collision (they are strictly newer).
    if let Some(archive_path) = archive_path {
        if archive_path.exists() {
            tracing::info!("Importing sessions-archive.jsonl into sessions.db (one-time, may take a while)…");
            use std::io::BufRead;
            let file = std::fs::File::open(&archive_path).map_err(|e| e.to_string())?;
            let reader = std::io::BufReader::new(file);
            for line in reader.lines() {
                let Ok(line) = line else { continue };
                if line.trim().is_empty() {
                    continue;
                }
                let Some(session) = session_from_json_str(&line) else {
                    tracing::warn!("Archive import: skipping unparseable line");
                    continue;
                };
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM sessions WHERE id = ?1",
                        params![session.id],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if exists {
                    continue;
                }
                let row = build_row(&session, next_seq()).map_err(|e| e.to_string())?;
                upsert_row(conn, &row).map_err(|e| e.to_string())?;
                imported_archive += 1;
            }
        }
    }

    meta_set_conn(conn, META_JSON_MIGRATED, &chrono::Utc::now().to_rfc3339())
        .map_err(|e| e.to_string())?;
    tracing::info!(
        "JSON→SQLite import complete: {imported_live} live sessions, {imported_archive} archived sessions. \
         The original JSON files were left in place and are no longer read."
    );
    Ok((imported_live, imported_archive))
}

/// Parse a single session JSON object, falling back to the legacy-format
/// migration path (old message formats from pre-v2 backups).
fn session_from_json_str(line: &str) -> Option<Session> {
    if let Ok(session) = serde_json::from_str::<Session>(line) {
        return Some(session);
    }
    // Wrap in a minimal SessionState shell and run the raw-JSON migrations.
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?.as_str()?.to_string();
    let shell = serde_json::json!({
        "sessions": { id.clone(): value },
        "active_session_id": id.clone(),
        "window_width": 0.0,
        "window_height": 0.0,
    });
    let shell_str = serde_json::to_string(&shell).ok()?;
    let state = crate::session::SessionState::migrate_from_raw_json(&shell_str).ok()?;
    state.sessions.into_values().next()
}

// ── Row building / upsert ───────────────────────────────────────────────────

pub struct SessionRow {
    pub id: String,
    pub name: String,
    pub last_updated: String,
    pub message_count: i64,
    pub total_cost: f64,
    pub total_tokens: i64,
    pub has_timers: bool,
    pub summary: String,
    pub search_text: String,
    pub seq: i64,
    pub data: String,
    pub data_hash: u64,
}

/// Fixed-width UTC timestamp so lexicographic string order == time order.
fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

pub fn build_row(session: &Session, seq: i64) -> Result<SessionRow, serde_json::Error> {
    let data = serde_json::to_string(session)?;
    let data_hash = hash_bytes(&data);
    let summary = session
        .active_context
        .conversation_summary
        .summary
        .clone();
    let mut search_text =
        String::with_capacity(session.name.len() + summary.len() + 64);
    search_text.push_str(&session.name.to_lowercase());
    search_text.push('\n');
    search_text.push_str(&summary.to_lowercase());
    for m in &session.messages {
        if let Some(t) = m.content.get_text_content() {
            search_text.push('\n');
            search_text.push_str(&t.to_lowercase());
        }
    }
    Ok(SessionRow {
        id: session.id.clone(),
        name: session.name.clone(),
        last_updated: fmt_ts(&session.last_updated),
        message_count: session.messages.len() as i64,
        total_cost: session.total_cost(),
        total_tokens: session.total_tokens() as i64,
        has_timers: !session.scheduled_timers.is_empty(),
        summary,
        search_text,
        seq,
        data,
        data_hash,
    })
}

fn upsert_row(conn: &Connection, row: &SessionRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sessions
            (id, name, last_updated, message_count, total_cost, total_tokens,
             has_timers, summary, search_text, seq, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             last_updated = excluded.last_updated,
             message_count = excluded.message_count,
             total_cost = excluded.total_cost,
             total_tokens = excluded.total_tokens,
             has_timers = excluded.has_timers,
             summary = excluded.summary,
             search_text = excluded.search_text,
             seq = excluded.seq,
             data = excluded.data
         WHERE excluded.seq >= sessions.seq",
        params![
            row.id,
            row.name,
            row.last_updated,
            row.message_count,
            row.total_cost,
            row.total_tokens,
            row.has_timers as i64,
            row.summary,
            row.search_text,
            row.seq,
            row.data,
        ],
    )?;
    Ok(())
}

/// Serialize every hydrated session, keep only the ones whose bytes changed
/// since the last successful write. Runs on the calling thread (cheap: open
/// sessions only). Fingerprints are committed by `mark_rows_saved` /
/// rolled back by `mark_rows_failed` after the actual write.
pub fn collect_dirty_rows(
    sessions: &HashMap<String, Session>,
) -> Result<Vec<SessionRow>, serde_json::Error> {
    let fps = fingerprints().lock().unwrap();
    let mut rows = Vec::new();
    for session in sessions.values() {
        let seq = next_seq();
        let row = build_row(session, seq)?;
        if fps.get(&row.id) == Some(&row.data_hash) {
            continue;
        }
        rows.push(row);
    }
    Ok(rows)
}

pub fn mark_rows_saved(rows: &[SessionRow]) {
    let mut fps = fingerprints().lock().unwrap();
    for row in rows {
        fps.insert(row.id.clone(), row.data_hash);
    }
}

pub fn mark_row_loaded(id: &str, data: &str) {
    fingerprints().lock().unwrap().insert(id.to_string(), hash_bytes(data));
}

pub fn forget_fingerprint(id: &str) {
    fingerprints().lock().unwrap().remove(id);
}

/// Generic fingerprint check for non-session blobs (e.g. tool-call history).
pub fn blob_changed(key: &str, data: &str) -> bool {
    fingerprints().lock().unwrap().get(key) != Some(&hash_bytes(data))
}

pub fn mark_blob_saved(key: &str, data: &str) {
    fingerprints().lock().unwrap().insert(key.to_string(), hash_bytes(data));
}

/// Write rows + meta entries. Called on a blocking thread.
pub fn write_rows_and_meta(rows: &[SessionRow], meta: &[(String, String)]) -> Result<(), String> {
    with_conn(|conn| {
        for row in rows {
            upsert_row(conn, row)?;
        }
        for (key, value) in meta {
            meta_set_conn(conn, key, value)?;
        }
        Ok(())
    })
}

// ── Meta ────────────────────────────────────────────────────────────────────

fn meta_set_conn(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, seq = excluded.seq
         WHERE excluded.seq >= meta.seq",
        params![key, value, next_seq()],
    )?;
    Ok(())
}

fn meta_get_conn(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
        .optional()
}

pub fn meta_get(key: &str) -> Option<String> {
    with_conn(|conn| meta_get_conn(conn, key)).ok().flatten()
}


// ── Queries ─────────────────────────────────────────────────────────────────

fn parse_session(id: &str, data: &str) -> Option<Session> {
    match serde_json::from_str::<Session>(data) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("Failed to parse session {id} directly ({e}); trying legacy migration");
            session_from_json_str(data)
        }
    }
}

/// Load specific sessions by id (hydration). Records fingerprints so the next
/// save doesn't rewrite unchanged rows.
pub fn load_sessions(ids: &[String]) -> HashMap<String, Session> {
    let mut out = HashMap::new();
    let result = with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT data FROM sessions WHERE id = ?1")?;
        for id in ids {
            use rusqlite::OptionalExtension;
            let data: Option<String> =
                stmt.query_row(params![id], |r| r.get(0)).optional()?;
            if let Some(data) = data {
                if let Some(session) = parse_session(id, &data) {
                    mark_row_loaded(id, &data);
                    out.insert(id.clone(), session);
                }
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        tracing::error!("load_sessions failed: {e}");
    }
    out
}

pub fn load_session(id: &str) -> Option<Session> {
    load_sessions(std::slice::from_ref(&id.to_string())).remove(id)
}

pub fn contains(id: &str) -> bool {
    with_conn(|conn| {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT 1 FROM sessions WHERE id = ?1", params![id], |_| Ok(()))
            .optional()
            .map(|o| o.is_some())
    })
    .unwrap_or(false)
}

pub fn session_ids_with_timers() -> Vec<String> {
    with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT id FROM sessions WHERE has_timers = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap_or_default()
}

pub fn most_recent_session_id() -> Option<String> {
    with_conn(|conn| {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT id FROM sessions ORDER BY last_updated DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
    })
    .ok()
    .flatten()
}

/// Delete a session row, returning its (total_cost, total_tokens) for
/// harvesting into the lifetime counters.
pub fn delete_session(id: &str) -> Option<(f64, i64)> {
    forget_fingerprint(id);
    with_conn(|conn| {
        use rusqlite::OptionalExtension;
        let harvest: Option<(f64, i64)> = conn
            .query_row(
                "SELECT total_cost, total_tokens FROM sessions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(harvest)
    })
    .ok()
    .flatten()
}

/// Rename a session directly in the DB (for sessions not hydrated in memory).
/// Rewrites the `name` field inside the data blob to keep them consistent.
pub fn rename_session(id: &str, new_name: &str) {
    let result = with_conn(|conn| {
        use rusqlite::OptionalExtension;
        let data: Option<String> = conn
            .query_row("SELECT data FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;
        let Some(data) = data else { return Ok(()) };
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&data) else {
            return Ok(());
        };
        value["name"] = serde_json::Value::String(new_name.to_string());
        let new_data = serde_json::to_string(&value).unwrap_or(data);
        conn.execute(
            "UPDATE sessions SET name = ?2, data = ?3, seq = ?4 WHERE id = ?1",
            params![id, new_name, new_data, next_seq()],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        tracing::error!("rename_session({id}) failed: {e}");
    }
}

/// Sum of cost/tokens across ALL stored sessions (live + never-opened).
pub fn sum_cost_tokens() -> (f64, i64) {
    with_conn(|conn| {
        conn.query_row(
            "SELECT COALESCE(SUM(total_cost), 0), COALESCE(SUM(total_tokens), 0) FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    })
    .unwrap_or((0.0, 0))
}

#[derive(Clone, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub last_updated: String,
    pub message_count: i64,
    pub summary: String,
}

/// Paginated, newest-first metadata listing. `query` (lowercased substring)
/// matches name, summary, or message text. Returns (page, total_matches).
pub fn list_metadata(query: Option<&str>, offset: i64, limit: i64) -> (Vec<SessionMeta>, i64) {
    let result = with_conn(|conn| {
        let (where_clause, pattern) = match query {
            Some(q) if !q.is_empty() => {
                let escaped = q.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                (
                    "WHERE (name LIKE ?1 ESCAPE '\\' COLLATE NOCASE \
                        OR summary LIKE ?1 ESCAPE '\\' COLLATE NOCASE \
                        OR search_text LIKE ?1 ESCAPE '\\')",
                    format!("%{escaped}%"),
                )
            }
            _ => ("", String::new()),
        };

        let count_sql = format!("SELECT COUNT(*) FROM sessions {where_clause}");
        let list_sql = format!(
            "SELECT id, name, last_updated, message_count, summary FROM sessions {where_clause} \
             ORDER BY last_updated DESC LIMIT {limit} OFFSET {offset}"
        );

        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<SessionMeta> {
            Ok(SessionMeta {
                id: r.get(0)?,
                name: r.get(1)?,
                last_updated: r.get(2)?,
                message_count: r.get(3)?,
                summary: r.get(4)?,
            })
        };

        if pattern.is_empty() {
            let total: i64 = conn.query_row(&count_sql, [], |r| r.get(0))?;
            let mut stmt = conn.prepare(&list_sql)?;
            let rows = stmt.query_map([], map_row)?;
            Ok((rows.collect::<rusqlite::Result<Vec<_>>>()?, total))
        } else {
            let total: i64 = conn.query_row(&count_sql, params![pattern], |r| r.get(0))?;
            let mut stmt = conn.prepare(&list_sql)?;
            let rows = stmt.query_map(params![pattern], map_row)?;
            Ok((rows.collect::<rusqlite::Result<Vec<_>>>()?, total))
        }
    });
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("list_metadata failed: {e}");
            (Vec::new(), 0)
        }
    }
}

/// All (id, name, summary) tuples for in-memory fuzzy matching. Cheap:
/// metadata only, no message blobs.
pub fn all_name_summaries() -> Vec<(String, String, String)> {
    with_conn(|conn| {
        let mut stmt =
            conn.prepare("SELECT id, name, summary FROM sessions ORDER BY last_updated DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .unwrap_or_default()
}

pub fn metadata_by_ids(ids: &[String]) -> Vec<SessionMeta> {
    with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name, last_updated, message_count, summary FROM sessions WHERE id = ?1",
        )?;
        let mut out = Vec::new();
        for id in ids {
            use rusqlite::OptionalExtension;
            if let Some(m) = stmt
                .query_row(params![id], |r| {
                    Ok(SessionMeta {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        last_updated: r.get(2)?,
                        message_count: r.get(3)?,
                        summary: r.get(4)?,
                    })
                })
                .optional()?
            {
                out.push(m);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

pub fn session_count() -> i64 {
    with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)))
        .unwrap_or(0)
}

/// Stream all raw session blobs for export, in insertion-agnostic id order.
/// Returns (id, data) pairs; caller assembles the export JSON incrementally
/// to avoid materializing a giant Value tree.
pub fn export_all_raw(mut per_row: impl FnMut(&str, &str)) -> Result<(), String> {
    with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT id, data FROM sessions ORDER BY last_updated")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let data: String = row.get(1)?;
            per_row(&id, &data);
        }
        Ok(())
    })
}

/// Insert a session only if its id is absent (import path).
pub fn insert_if_absent(session: &Session) -> bool {
    if contains(&session.id) {
        return false;
    }
    let Ok(row) = build_row(session, next_seq()) else { return false };
    with_conn(|conn| upsert_row(conn, &row)).is_ok()
}

// ── Test support ────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// Run store logic against an isolated in-memory database.
    pub fn with_test_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        f(&conn)
    }

    pub fn upsert(conn: &Connection, session: &Session) {
        let row = build_row(session, next_seq()).unwrap();
        upsert_row(conn, &row).unwrap();
    }

    pub fn upsert_with_seq(conn: &Connection, session: &Session, seq: i64) {
        let row = build_row(session, seq).unwrap();
        upsert_row(conn, &row).unwrap();
    }

    pub fn get_row_name(conn: &Connection, id: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT name FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .unwrap()
    }

    pub fn run_import(
        conn: &Connection,
        json_path: Option<std::path::PathBuf>,
        archive_path: Option<std::path::PathBuf>,
    ) -> Result<(usize, usize), String> {
        import_json_files(conn, json_path, archive_path)
    }

    pub fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap()
    }

    pub fn meta(conn: &Connection, key: &str) -> Option<String> {
        meta_get_conn(conn, key).unwrap()
    }

    pub fn get_row_data(conn: &Connection, id: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT data FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .unwrap()
    }
}
