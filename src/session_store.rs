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
use crate::session_events::SessionEvent;

static CONN: OnceLock<Mutex<Connection>> = OnceLock::new();
static FINGERPRINTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
/// Deleted session ids → the seq at which the delete happened. Background
/// saves carry rows collected *before* a delete; without this, such a write
/// would re-insert the deleted row (the seq upsert guard only protects
/// updates, and no save path deletes rows). Rows whose seq predates the
/// tombstone are dropped at write time.
static TOMBSTONES: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
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

fn tombstones() -> &'static Mutex<HashMap<String, i64>> {
    TOMBSTONES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn hash_bytes(data: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

pub(crate) fn next_seq() -> i64 {
    SEQ.fetch_add(1, Ordering::SeqCst)
}

/// Seed the seq counter from the store's high-water mark. SEQ is process-local;
/// without this a fresh process starts below rows written by a longer-lived
/// earlier process, and the `excluded.seq >= sessions.seq` / `meta.seq` upsert
/// guards silently reject every update to those rows and meta keys until the
/// counter catches up — silent data loss that only shows after a restart.
/// `fetch_max` only ever moves the counter forward (re-init, tests).
fn seed_seq_from_db(conn: &Connection) {
    let sessions_max: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    let meta_max: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM meta", [], |r| r.get(0))
        .unwrap_or(0);
    let events_max: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM session_events", [], |r| r.get(0))
        .unwrap_or(0);
    SEQ.fetch_max(sessions_max.max(meta_max).max(events_max) + 1, Ordering::SeqCst);
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
         );
         CREATE TABLE IF NOT EXISTS session_events (
             session_id TEXT NOT NULL,
             seq        INTEGER NOT NULL,
             ts         TEXT NOT NULL,
             kind       TEXT NOT NULL,
             payload    TEXT NOT NULL,
             PRIMARY KEY (session_id, seq)
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

    seed_seq_from_db(&conn);

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

    // The whole import — rows, meta, and the migrated marker — commits
    // atomically: a crash mid-import leaves the marker unset, so the import
    // retries next launch instead of persisting a partial history.
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let conn = &*tx;

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
                meta_set_conn(conn, META_SCHEMA_VERSION, &state.schema_version.to_string(), next_seq())?;
                meta_set_conn(conn, META_ACTIVE_SESSION, &state.active_session_id, next_seq())?;
                meta_set_conn(conn, META_WINDOW_WIDTH, &state.window_width.to_string(), next_seq())?;
                meta_set_conn(conn, META_WINDOW_HEIGHT, &state.window_height.to_string(), next_seq())?;
                meta_set_conn(conn, META_LIFETIME_COST, &state.lifetime_cost.to_string(), next_seq())?;
                meta_set_conn(conn, META_LIFETIME_TOKENS, &state.lifetime_tokens.to_string(), next_seq())?;
                let tch = serde_json::to_string(&state.tool_call_history).unwrap_or_else(|_| "[]".into());
                meta_set_conn(conn, META_TOOL_CALL_HISTORY, &tch, next_seq())?;
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

    meta_set_conn(conn, META_JSON_MIGRATED, &chrono::Utc::now().to_rfc3339(), next_seq())
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
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
    let tombs = tombstones().lock().unwrap();
    let mut fps = fingerprints().lock().unwrap();
    for row in rows {
        // A tombstoned row was dropped by write_rows_and_meta, not saved —
        // recording its fingerprint would mask a future re-import.
        if tombs.get(&row.id).is_some_and(|del_seq| row.seq < *del_seq) {
            continue;
        }
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

/// Write rows + meta entries atomically. Called on a blocking thread.
/// Meta seqs are assigned at collect time (see `collect_meta_kv`) so an older
/// snapshot whose write lands later cannot overwrite newer meta. Rows whose
/// seq predates a delete tombstone are dropped — they were collected before
/// the session was deleted and must not resurrect it.
pub fn write_rows_and_meta(rows: &[SessionRow], meta: &[(String, String, i64)]) -> Result<(), String> {
    with_conn(|conn| {
        let tx = conn.unchecked_transaction()?;
        {
            let mut tombs = tombstones().lock().unwrap();
            for row in rows {
                match tombs.get(&row.id) {
                    Some(del_seq) if row.seq < *del_seq => continue,
                    Some(_) => {
                        // Row collected after the delete — the id was
                        // re-created, so the tombstone no longer applies.
                        tombs.remove(&row.id);
                    }
                    None => {}
                }
                upsert_row(&tx, row)?;
            }
        }
        for (key, value, seq) in meta {
            meta_set_conn(&tx, key, value, *seq)?;
        }
        tx.commit()
    })
}

// ── Meta ────────────────────────────────────────────────────────────────────

fn meta_set_conn(conn: &Connection, key: &str, value: &str, seq: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, seq = excluded.seq
         WHERE excluded.seq >= meta.seq",
        params![key, value, seq],
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
/// harvesting into the lifetime counters. Also deletes the session's
/// event journal — events must not outlive their session.
pub fn delete_session(id: &str) -> Option<(f64, i64)> {
    forget_fingerprint(id);
    tombstones().lock().unwrap().insert(id.to_string(), next_seq());
    with_conn(|conn| delete_session_conn(conn, id)).ok().flatten()
}

fn delete_session_conn(conn: &Connection, id: &str) -> rusqlite::Result<Option<(f64, i64)>> {
    use rusqlite::OptionalExtension;
    let harvest: Option<(f64, i64)> = conn
        .query_row(
            "SELECT total_cost, total_tokens FROM sessions WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM session_events WHERE session_id = ?1", params![id])?;
    Ok(harvest)
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

// ── Session event journal (append-only, Phase 1: write-only) ───────────────

/// A serialized event ready for insertion. Built on the calling thread
/// (P-009); only these owned buffers move to the background writer.
pub struct SessionEventRow {
    pub session_id: String,
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub payload: String,
}

/// An event read back from the journal.
#[derive(Debug, Clone)]
pub struct LoadedSessionEvent {
    pub seq: i64,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub event: SessionEvent,
}

/// Serialize events into rows on the calling thread. Seqs come from the same
/// process-wide counter as session/meta writes, so journal order interleaves
/// correctly with row saves. The row `ts` is captured here, at append time —
/// replay never needs a clock.
pub fn prepare_event_rows(session_id: &str, events: &[SessionEvent]) -> Vec<SessionEventRow> {
    let ts = fmt_ts(&chrono::Utc::now());
    events
        .iter()
        .filter_map(|event| match serde_json::to_string(event) {
            Ok(payload) => Some(SessionEventRow {
                session_id: session_id.to_string(),
                seq: next_seq(),
                ts: ts.clone(),
                kind: event.kind().to_string(),
                payload,
            }),
            Err(e) => {
                tracing::error!("session_events: failed to serialize {} event: {e}", event.kind());
                None
            }
        })
        .collect()
}

fn insert_event_rows_conn(conn: &Connection, rows: &[SessionEventRow]) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO session_events (session_id, seq, ts, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for row in rows {
            stmt.execute(params![row.session_id, row.seq, row.ts, row.kind, row.payload])?;
        }
    }
    tx.commit()
}

/// Write prepared event rows. Called on a blocking thread by `append_events`,
/// or directly by non-async callers.
pub fn write_event_rows(rows: &[SessionEventRow]) -> Result<(), String> {
    with_conn(|conn| insert_event_rows_conn(conn, rows))
}

/// Append events to a session's journal (batch-friendly; a turn can produce
/// several). Serializes on the calling thread, then moves only the prepared
/// rows to a background blocking task — or writes inline when no tokio
/// runtime is available (tests, non-async callers). Failures are logged and
/// swallowed: the journal is dual-write-only in this phase and must never
/// affect app behavior.
pub fn append_events(session_id: &str, events: Vec<SessionEvent>) {
    if events.is_empty() {
        return;
    }
    if CONN.get().is_none() {
        tracing::debug!("session_events: store not initialized, dropping {} event(s)", events.len());
        return;
    }
    let rows = prepare_event_rows(session_id, &events);
    if rows.is_empty() {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let result = tokio::task::spawn_blocking(move || write_event_rows(&rows)).await;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::error!("session_events: append failed: {e}"),
                    Err(e) => tracing::error!("session_events: append task panicked: {e}"),
                }
            });
        }
        Err(_) => {
            if let Err(e) = write_event_rows(&rows) {
                tracing::error!("session_events: append failed: {e}");
            }
        }
    }
}

fn load_events_conn(
    conn: &Connection,
    session_id: &str,
    after_seq: i64,
) -> rusqlite::Result<Vec<LoadedSessionEvent>> {
    let mut stmt = conn.prepare(
        "SELECT seq, ts, kind, payload FROM session_events
         WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
    )?;
    let mut rows = stmt.query(params![session_id, after_seq])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let ts_str: String = row.get(1)?;
        let kind: String = row.get(2)?;
        let payload: String = row.get(3)?;
        // Version tolerance: rows are deserialized individually. A kind this
        // build doesn't know (written by a newer version) — or a corrupt
        // payload — is skipped with a warning, never fails the whole read.
        let event = match serde_json::from_str::<SessionEvent>(&payload) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(
                    "session_events: skipping unreadable event (session={session_id}, seq={seq}, kind={kind}): {e}"
                );
                continue;
            }
        };
        let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_default();
        out.push(LoadedSessionEvent { seq, ts, event });
    }
    Ok(out)
}

/// Read a session's journal in seq order, skipping unknown kinds.
/// `after_seq = 0` reads from the beginning.
pub fn load_events(session_id: &str, after_seq: i64) -> Vec<LoadedSessionEvent> {
    with_conn(|conn| load_events_conn(conn, session_id, after_seq)).unwrap_or_else(|e| {
        tracing::error!("session_events: load failed for {session_id}: {e}");
        Vec::new()
    })
}

/// The seq of the earliest journaled event referencing a message id — the
/// rewind anchor for `RewoundTo`. Substring match on the payload is exact
/// enough: message ids are UUIDs.
pub fn first_event_seq_for_message(session_id: &str, message_id: &str) -> Option<i64> {
    with_conn(|conn| {
        conn.query_row(
            "SELECT MIN(seq) FROM session_events WHERE session_id = ?1 AND payload LIKE ?2",
            params![session_id, format!("%{message_id}%")],
            |r| r.get::<_, Option<i64>>(0),
        )
    })
    .ok()
    .flatten()
}

/// A session is "journal-complete" iff its journal starts with a
/// `SessionCreated` event — only then can it be rebuilt from nothing via
/// `session_events::project(None, …)`. Pre-journal sessions (created before
/// the birth event existed) fail this and keep the legacy code paths.
pub fn journal_starts_with_creation(session_id: &str) -> bool {
    with_conn(|conn| journal_starts_with_creation_conn(conn, session_id))
        .unwrap_or(false)
}

fn journal_starts_with_creation_conn(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
    let first_kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM session_events WHERE session_id = ?1 ORDER BY seq LIMIT 1",
            params![session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(first_kind.as_deref() == Some("SessionCreated"))
}

/// Fork a journal-complete session's history (Phase 2, Part D).
///
/// Copies `source_id`'s events with seq ≤ `at_seq` (inclusive; `None` = all)
/// into `new_id` with NEW seqs from the shared counter, rewriting the copied
/// `SessionCreated` to the new identity and appending a `SessionForked`
/// provenance marker. Row timestamps are preserved from the source rows (the
/// marker gets append-time now). Returns the new journal (with its new seqs)
/// so the caller can `project()` it without a read-back.
///
/// Errors when the source journal is empty or does not start with
/// `SessionCreated` (pre-journal session).
pub fn fork_events(
    source_id: &str,
    at_seq: Option<i64>,
    new_id: &str,
    new_name: &str,
) -> Result<Vec<LoadedSessionEvent>, String> {
    with_conn(|conn| fork_events_conn(conn, source_id, at_seq, new_id, new_name))?
}

fn fork_events_conn(
    conn: &Connection,
    source_id: &str,
    at_seq: Option<i64>,
    new_id: &str,
    new_name: &str,
) -> rusqlite::Result<Result<Vec<LoadedSessionEvent>, String>> {
    let mut source = load_events_conn(conn, source_id, 0)?;
    if let Some(cut) = at_seq {
        source.retain(|e| e.seq <= cut);
    }

    match source.first() {
        Some(first) if matches!(first.event, SessionEvent::SessionCreated { .. }) => {}
        _ => {
            return Ok(Err(format!(
                "Session '{source_id}' predates the event journal (no SessionCreated) — fork unavailable."
            )));
        }
    }

    let last_copied_seq = source.last().map(|e| e.seq).unwrap_or(0);
    let last_ts = source.last().map(|e| e.ts).unwrap_or_default();
    let had_rename = source
        .iter()
        .any(|e| matches!(e.event, SessionEvent::SessionRenamed { .. }));

    // Rewrite the birth event to the fork's identity.
    if let Some(first) = source.first_mut() {
        first.event = SessionEvent::SessionCreated {
            id: new_id.to_string(),
            name: new_name.to_string(),
        };
    }
    // A later copied SessionRenamed would clobber the fork's name on replay —
    // re-assert it when the prefix contains one.
    if had_rename {
        source.push(LoadedSessionEvent {
            seq: 0, // rewritten below
            ts: last_ts,
            event: SessionEvent::SessionRenamed { name: new_name.to_string() },
        });
    }
    // Provenance marker, timestamped at fork time.
    source.push(LoadedSessionEvent {
        seq: 0, // rewritten below
        ts: chrono::Utc::now(),
        event: SessionEvent::SessionForked {
            from_session_id: source_id.to_string(),
            at_seq: at_seq.unwrap_or(last_copied_seq),
        },
    });

    // Mint new seqs from the shared counter and insert under the new id,
    // preserving each row's original timestamp.
    let mut rows = Vec::with_capacity(source.len());
    for loaded in source.iter_mut() {
        loaded.seq = next_seq();
        match serde_json::to_string(&loaded.event) {
            Ok(payload) => rows.push(SessionEventRow {
                session_id: new_id.to_string(),
                seq: loaded.seq,
                ts: fmt_ts(&loaded.ts),
                kind: loaded.event.kind().to_string(),
                payload,
            }),
            Err(e) => tracing::error!(
                "fork_events: failed to serialize {} event: {e}",
                loaded.event.kind()
            ),
        }
    }
    insert_event_rows_conn(conn, &rows)?;
    Ok(Ok(source))
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

    pub fn seed_seq(conn: &Connection) {
        seed_seq_from_db(conn);
    }

    pub fn meta_set(conn: &Connection, key: &str, value: &str) {
        meta_set_conn(conn, key, value, next_seq()).unwrap();
    }

    pub fn meta_set_with_seq(conn: &Connection, key: &str, value: &str, seq: i64) {
        conn.execute(
            "INSERT INTO meta (key, value, seq) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, seq = excluded.seq
             WHERE excluded.seq >= meta.seq",
            params![key, value, seq],
        )
        .unwrap();
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

    // ── Event journal test helpers (conn-parameterized) ─────────────────────

    pub fn append_events_conn(conn: &Connection, session_id: &str, events: &[SessionEvent]) {
        let rows = prepare_event_rows(session_id, events);
        insert_event_rows_conn(conn, &rows).unwrap();
    }

    pub fn load_events_for_test(
        conn: &Connection,
        session_id: &str,
        after_seq: i64,
    ) -> Vec<LoadedSessionEvent> {
        load_events_conn(conn, session_id, after_seq).unwrap()
    }

    /// Insert a raw journal row directly (e.g. an unknown future kind).
    pub fn insert_raw_event(conn: &Connection, session_id: &str, kind: &str, payload: &str) {
        insert_raw_event_with_seq(conn, session_id, next_seq(), kind, payload);
    }

    pub fn insert_raw_event_with_seq(
        conn: &Connection,
        session_id: &str,
        seq: i64,
        kind: &str,
        payload: &str,
    ) {
        conn.execute(
            "INSERT INTO session_events (session_id, seq, ts, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, seq, fmt_ts(&chrono::Utc::now()), kind, payload],
        )
        .unwrap();
    }

    pub fn event_count(conn: &Connection, session_id: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_events WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    pub fn delete_session_for_test(conn: &Connection, id: &str) -> Option<(f64, i64)> {
        delete_session_conn(conn, id).unwrap()
    }

    pub fn fork_events_for_test(
        conn: &Connection,
        source_id: &str,
        at_seq: Option<i64>,
        new_id: &str,
        new_name: &str,
    ) -> Result<Vec<LoadedSessionEvent>, String> {
        fork_events_conn(conn, source_id, at_seq, new_id, new_name).unwrap()
    }

    pub fn journal_complete_for_test(conn: &Connection, session_id: &str) -> bool {
        journal_starts_with_creation_conn(conn, session_id).unwrap()
    }
}

// ── Event journal tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod event_tests {
    use super::test_support::*;
    use super::*;
    use crate::components::chat::Message;
    use crate::components::shared::MessageContent;
    use crate::timers::{ScheduledTimer, TimerMode, TimerStatus};

    fn text_message(author: &str, content: &str) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.to_string(),
            content: MessageContent::Text {
                content: content.to_string(),
                thought_signature: None,
                thought_summary: None,
            },
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: chrono::Utc::now(),
            usage: None,
        }
    }

    fn tool_call_message(status: crate::components::shared::ToolCallStatus) -> Message {
        let mut tc = crate::components::shared::ToolCall::new(
            "server".to_string(),
            "TOOL_NAME".to_string(),
            serde_json::json!({"arg": 1}),
            None,
            None,
        );
        tc.status = status;
        Message {
            id: uuid::Uuid::new_v4(),
            author: "Hobbes".to_string(),
            content: MessageContent::ToolCall(tc),
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: chrono::Utc::now(),
            usage: None,
        }
    }

    fn timer(id: &str, session_id: &str) -> ScheduledTimer {
        let now = chrono::Utc::now();
        ScheduledTimer {
            id: id.to_string(),
            session_id: session_id.to_string(),
            created_at: now,
            fire_at: now + chrono::Duration::seconds(60),
            mode: TimerMode::Notify,
            label: Some("test".to_string()),
            prompt: None,
            status: TimerStatus::Pending,
        }
    }

    /// Every variant survives an append → load round-trip, in order.
    #[test]
    fn test_event_roundtrip_all_variants() {
        let events = vec![
            SessionEvent::UserMessage { message: text_message("User", "hi") },
            SessionEvent::AssistantMessage { message: text_message("Hobbes", "hello") },
            SessionEvent::ToolCall {
                message: tool_call_message(crate::components::shared::ToolCallStatus::Running),
            },
            SessionEvent::ToolResult {
                message: tool_call_message(crate::components::shared::ToolCallStatus::Completed),
            },
            SessionEvent::ScratchpadSet { content: "notes".to_string() },
            SessionEvent::SkillLoaded {
                name: "my-skill".to_string(),
                payload: "{\"skill\":\"my-skill\"}".to_string(),
            },
            SessionEvent::SkillUnloaded { name: "my-skill".to_string() },
            SessionEvent::SummaryComputed {
                summary: serde_json::json!({"summary": "we talked", "sentiment": "good"}),
            },
            SessionEvent::TimerCreated { timer: timer("tmr_1", "s1") },
            SessionEvent::TimerCancelled { timer_id: "tmr_1".to_string() },
            SessionEvent::TimerFired { timer: timer("tmr_2", "s1") },
            SessionEvent::ConnectorPinned {
                connector_id: Some("conn-1".to_string()),
                provider: Some("Gemini".to_string()),
                model: Some("gemini-pro".to_string()),
            },
            SessionEvent::SessionRenamed { name: "New Name".to_string() },
            SessionEvent::RewoundTo { seq: 7, message_id: "abc".to_string() },
        ];

        with_test_db(|conn| {
            append_events_conn(conn, "s1", &events);
            let loaded = load_events_for_test(conn, "s1", 0);
            assert_eq!(loaded.len(), events.len());
            for (got, want) in loaded.iter().zip(events.iter()) {
                assert_eq!(&got.event, want);
            }
            // seqs strictly ascending
            for pair in loaded.windows(2) {
                assert!(pair[0].seq < pair[1].seq);
            }
            // after_seq filters
            let after = load_events_for_test(conn, "s1", loaded[2].seq);
            assert_eq!(after.len(), events.len() - 3);
            assert_eq!(after[0].event, events[3]);
        });
    }

    /// Seqs come from the shared process-wide counter: interleaved appends to
    /// two sessions still order globally by append time.
    #[test]
    fn test_event_seq_monotonic_across_sessions() {
        with_test_db(|conn| {
            append_events_conn(conn, "a", &[SessionEvent::SessionRenamed { name: "1".into() }]);
            append_events_conn(conn, "b", &[SessionEvent::SessionRenamed { name: "2".into() }]);
            append_events_conn(conn, "a", &[SessionEvent::SessionRenamed { name: "3".into() }]);

            let a = load_events_for_test(conn, "a", 0);
            let b = load_events_for_test(conn, "b", 0);
            assert_eq!(a.len(), 2);
            assert_eq!(b.len(), 1);
            assert!(a[0].seq < b[0].seq, "first a before b");
            assert!(b[0].seq < a[1].seq, "b before second a");
        });
    }

    /// A row whose kind this build doesn't know is skipped, not fatal — and
    /// rows around it still load.
    #[test]
    fn test_unknown_kind_row_is_skipped() {
        with_test_db(|conn| {
            append_events_conn(conn, "s1", &[SessionEvent::ScratchpadSet { content: "a".into() }]);
            insert_raw_event(
                conn,
                "s1",
                "TeleportedFromTheFuture",
                r#"{"kind":"TeleportedFromTheFuture","wormhole":true}"#,
            );
            append_events_conn(conn, "s1", &[SessionEvent::ScratchpadSet { content: "b".into() }]);

            assert_eq!(event_count(conn, "s1"), 3);
            let loaded = load_events_for_test(conn, "s1", 0);
            assert_eq!(loaded.len(), 2, "unknown kind skipped, not fatal");
            assert_eq!(
                loaded[0].event,
                SessionEvent::ScratchpadSet { content: "a".into() }
            );
            assert_eq!(
                loaded[1].event,
                SessionEvent::ScratchpadSet { content: "b".into() }
            );
        });
    }

    /// Deleting a session deletes its journal too.
    #[test]
    fn test_delete_session_removes_events() {
        with_test_db(|conn| {
            let mut state = crate::session::SessionState::default();
            let sid = state.create_session_raw(None);
            upsert(conn, state.sessions.get(&sid).unwrap());
            // A second session that must keep its events.
            let other = state.create_session_raw(None);
            upsert(conn, state.sessions.get(&other).unwrap());

            append_events_conn(conn, &sid, &[SessionEvent::ScratchpadSet { content: "x".into() }]);
            append_events_conn(conn, &other, &[SessionEvent::ScratchpadSet { content: "y".into() }]);
            assert_eq!(event_count(conn, &sid), 1);

            delete_session_for_test(conn, &sid);
            assert_eq!(event_count(conn, &sid), 0, "deleted session's events removed");
            assert_eq!(event_count(conn, &other), 1, "other session's events survive");
            assert!(get_row_name(conn, &sid).is_none());
        });
    }

    /// seed_seq must account for the events table's high-water mark, or a
    /// fresh process would mint seqs below already-journaled rows.
    #[test]
    fn test_seed_seq_accounts_for_events_high_water() {
        with_test_db(|conn| {
            let high = next_seq() + 100_000;
            insert_raw_event_with_seq(
                conn,
                "s1",
                high,
                "ScratchpadSet",
                r#"{"kind":"ScratchpadSet","content":"from a longer-lived process"}"#,
            );
            seed_seq(conn);
            let fresh = next_seq();
            assert!(
                fresh > high,
                "seq counter must clear the events high-water mark (got {fresh}, high {high})"
            );
        });
    }
}
