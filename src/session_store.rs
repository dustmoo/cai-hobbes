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
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
/// Legacy: the pre-per-session global tool-call-history blob. No longer
/// written; `SessionState::load` deletes any surviving row.
pub const META_TOOL_CALL_HISTORY: &str = "tool_call_history";
const META_JSON_MIGRATED: &str = "migrated_from_json";
/// Set once `search_text` carries each row's AI topic title (`tab_title`).
const META_SEARCH_TAB_TITLE: &str = "search_text_has_tab_title";
/// Set once the `project_id` column carries each row's `data.project_id`.
const META_PROJECT_ID_COLUMN: &str = "project_id_column_backfilled";
/// Set once the background `tab_title` backfill (`spawn_tab_title_backfill`) has filled the column.
const META_TAB_TITLE_COLUMN: &str = "tab_title_column_backfilled";
/// The one-time pin backfill over stored rows has finished.
const META_PIN_BACKFILLED: &str = "unpinned_tabs_pinned";
/// The last rowid the pin backfill finished, so a restart resumes there.
const META_PIN_CURSOR: &str = "unpinned_tabs_pin_cursor";

/// Bumped whenever a write changes what the History listing shows (a row
/// added, deleted or renamed, or its title, emoji, summary, message count or
/// timestamp changed), so the History drawer re-queries on that and not on
/// every save of a streaming reply. See [`list_version`].
static LIST_VERSION: AtomicU64 = AtomicU64::new(0);
/// Per row: the hash of its listed fields as last written ([`SessionRow::list_hash`]).
static LIST_KEYS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

/// A counter that changes whenever the History listing may have changed.
/// Compare against a previously read value; the number itself means nothing.
pub fn list_version() -> u64 {
    LIST_VERSION.load(Ordering::Acquire)
}

fn list_watch() -> &'static tokio::sync::watch::Sender<u64> {
    static TX: OnceLock<tokio::sync::watch::Sender<u64>> = OnceLock::new();
    TX.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// A receiver that wakes whenever [`list_version`] is bumped. Await
/// `changed()`, then re-read [`list_version`] for the value.
pub fn subscribe_list_version() -> tokio::sync::watch::Receiver<u64> {
    list_watch().subscribe()
}

fn bump_list_version() {
    let v = LIST_VERSION.fetch_add(1, Ordering::AcqRel) + 1;
    list_watch().send_replace(v);
}

/// Bump [`list_version`] if `id`'s listed fields differ from its last write.
fn note_listing(id: &str, list_hash: u64) {
    let mut keys = LIST_KEYS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap();
    if keys.insert(id.to_string(), list_hash) != Some(list_hash) {
        bump_list_version();
    }
}

/// `id`'s row was deleted or changed outside `upsert_row`: always re-list.
fn forget_listing(id: &str) {
    if let Some(keys) = LIST_KEYS.get() {
        keys.lock().unwrap().remove(id);
    }
    bump_list_version();
}

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
    // The planner's, fleet's, and session-journal's tables share this counter —
    // omitting any of them would restart the seq below their existing rows and
    // silently reject every subsequent write in that domain.
    let todo_max = crate::todo::store::max_seq(conn);
    let fleet_max = crate::fleet::store::max_seq(conn);
    let trust_max = crate::context::trust_store::max_seq(conn);
    let events_max: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM session_events", [], |r| r.get(0))
        .unwrap_or(0);
    SEQ.fetch_max(
        sessions_max
            .max(meta_max)
            .max(todo_max)
            .max(fleet_max)
            .max(trust_max)
            .max(events_max)
            + 1,
        Ordering::SeqCst,
    );
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
         -- A WAL file keeps its high-water size after a checkpoint unless
         -- this is set; one large write (a backfill, an import) otherwise
         -- leaves hundreds of MB on disk for good.
         PRAGMA journal_size_limit=33554432;
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
             data          TEXT NOT NULL,
             project_id    TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_sessions_last_updated ON sessions(last_updated DESC);
         -- `seed_seq_from_db` reads MAX(seq) at every launch. Without this
         -- it walks every row, through each row's search_text overflow
         -- pages to reach the column: ~0.5 s cold on a 1.5 GB store, before
         -- the window shows. Built once (one such walk) on first open.
         CREATE INDEX IF NOT EXISTS idx_sessions_seq ON sessions(seq);
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
    )?;
    ensure_project_id_column(conn)?;
    ensure_tab_title_column(conn)?;

    // The planner and the fleet share this database and connection. Creating
    // their schemas here means test databases get their tables too.
    crate::todo::store::create_schema(conn)?;
    crate::fleet::store::create_schema(conn)?;
    crate::context::trust_store::create_schema(conn)
}

/// Open the global connection, create the schema, and run the one-time
/// import from `sessions.json` + `sessions-archive.jsonl`. Idempotent;
/// safe to call from both `main()` and `app()`.
pub fn init() -> Result<(), String> {
    if CONN.get().is_some() {
        return Ok(());
    }
    let opened = std::time::Instant::now();
    let path = get_db_path().ok_or("could not resolve sessions.db path")?;
    let conn = open_store(&path, env!("CARGO_PKG_VERSION"))?;
    crate::startup_timing::record("store open", opened);

    let migrations = std::time::Instant::now();
    if let Err(e) = migrate_from_json_files(&conn) {
        // Import failure must not brick the app, but log loudly — the JSON
        // files are left untouched so the import retries next launch.
        tracing::error!("One-time JSON→SQLite import failed (will retry next launch): {e}");
    }

    if let Err(e) = backfill_search_tab_titles(&conn) {
        tracing::warn!("search_text tab_title backfill failed (will retry next launch): {e}");
    }

    if let Err(e) = backfill_project_id_column(&conn) {
        tracing::warn!("project_id column backfill failed (will retry next launch): {e}");
    }

    crate::startup_timing::record("store migrations", migrations);

    crate::startup_timing::time("store seq", || seed_seq_from_db(&conn));

    let _ = CONN.set(Mutex::new(conn));
    spawn_tab_title_backfill();
    Ok(())
}

pub fn is_available() -> bool {
    CONN.get().is_some()
}

/// Open the store at `path` for app `version`: owner-only permissions, the
/// once-per-version backup ([`backup_before_upgrade`]) — before anything
/// changes the file — then the schema. Everything after (imports,
/// backfills) is `init`'s. A failed backup never fails the open.
fn open_store(path: &std::path::Path, version: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;

    // Restrict to owner-only, matching the old sessions.json (0600). SQLite
    // copies the db file's mode when it creates the -wal/-shm companions, so
    // setting it here covers those too; chmod any that already exist.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.to_path_buf().into_os_string();
            p.push(suffix);
            let p = std::path::PathBuf::from(p);
            if p.exists() {
                if let Err(e) = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)) {
                    tracing::warn!("Failed to set permissions on {}: {e}", p.display());
                }
            }
        }
    }

    let backup = crate::startup_timing::time("store backup", || backup_before_upgrade(&conn, path, version));
    create_schema(&conn).map_err(|e| format!("schema: {e}"))?;
    if backup == BackupOutcome::FreshInstall {
        // Nothing to back up for this version: the next launch of the same
        // version mustn't take one either.
        if let Err(e) = meta_set_conn(&conn, META_BACKUP_VERSION, version, 0) {
            tracing::warn!("store backup: could not record the version: {e}");
        }
    }
    Ok(conn)
}

// ── Pre-upgrade backup ──────────────────────────────────────────────────────

/// The app version `sessions.db` was last backed up for (or created by).
const META_BACKUP_VERSION: &str = "store_backed_up_for_version";
/// Set to the version whose backup failed: its migrations ran without one,
/// so the next launch's backup postdates them.
const META_BACKUP_MISSED: &str = "store_backup_missed_version";

/// What [`backup_before_upgrade`] did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BackupOutcome {
    /// A new database: nothing to back up.
    FreshInstall,
    /// Already backed up for this version.
    UpToDate,
    Taken { path: PathBuf, bytes: u64 },
    /// Logged and skipped; the next launch tries again.
    Failed,
}

/// `<db file name>.pre-<version>.bak`, next to the database.
fn backup_path(db_path: &std::path::Path, version: &str) -> PathBuf {
    let name = db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    db_path.with_file_name(format!("{name}.pre-{version}.bak"))
}

/// The backups (`<db>.pre-*.bak`) and stray temp files (`<db>.pre-*.bak.tmp`)
/// next to `db_path`. A temp file is never a backup.
fn backup_files(db_path: &std::path::Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let name = db_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let prefix = format!("{name}.pre-");
    let (mut baks, mut tmps) = (Vec::new(), Vec::new());
    let Some(dir) = db_path.parent() else { return (baks, tmps) };
    let Ok(entries) = std::fs::read_dir(dir) else { return (baks, tmps) };
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.starts_with(&prefix) {
            continue;
        }
        if file.ends_with(".bak") {
            baks.push(entry.path());
        } else if file.ends_with(".bak.tmp") {
            tmps.push(entry.path());
        }
    }
    (baks, tmps)
}

/// Once per app version, before any schema change, migration or backfill:
/// a consistent copy of the database at `db_path` as
/// `<db>.pre-<version>.bak`, keeping only the newest. Runs on the store's
/// only connection, before it is shared.
///
/// Consistent: a `TRUNCATE` checkpoint moves the WAL into the main file,
/// then a write transaction (`BEGIN IMMEDIATE`) keeps any other writer out
/// while the main file is copied, and the copy is rejected if the WAL grew
/// in between. The copy is `std::fs::copy`: a clone on APFS (near-instant,
/// no extra disk until the files diverge), a full copy on Windows. It goes
/// to a `.tmp` name, is synced, then renamed, so a crash never leaves a
/// partial file named like a backup.
///
/// Never blocks startup: on failure it logs a WARN (no paths or content),
/// records the version as missed and leaves the done marker alone, so the
/// next launch tries again. That retry runs after this version's
/// migrations, so it keeps the older backups too (they predate them) and
/// says so; the next version's backup prunes them.
pub(crate) fn backup_before_upgrade(conn: &Connection, db_path: &std::path::Path, version: &str) -> BackupOutcome {
    let tables: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0))
        .unwrap_or(0);
    if tables == 0 {
        return BackupOutcome::FreshInstall;
    }
    let has_meta = conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |_| Ok(()))
            .is_ok();
    let (done, missed) = if has_meta {
        (
            meta_get_conn(conn, META_BACKUP_VERSION).ok().flatten(),
            meta_get_conn(conn, META_BACKUP_MISSED).ok().flatten(),
        )
    } else {
        (None, None)
    };
    if done.as_deref() == Some(version) {
        return BackupOutcome::UpToDate;
    }
    let started = std::time::Instant::now();
    let after_migrations = missed.as_deref() == Some(version);
    match take_backup(conn, db_path, version) {
        Ok((path, bytes)) => {
            let (baks, _) = backup_files(db_path);
            if after_migrations {
                tracing::warn!(
                    version,
                    "store backup: taken after this version's migrations (last launch's backup failed); older backups kept"
                );
            } else {
                for old in baks.iter().filter(|b| **b != path) {
                    if let Err(e) = std::fs::remove_file(old) {
                        tracing::warn!(kind = ?e.kind(), "store backup: could not remove an older backup");
                    }
                }
            }
            if has_meta {
                let recorded = meta_set_conn(conn, META_BACKUP_VERSION, version, 0)
                    .and_then(|_| conn.execute("DELETE FROM meta WHERE key = ?1", params![META_BACKUP_MISSED]).map(|_| ()));
                if let Err(e) = recorded {
                    tracing::warn!("store backup: taken, but the version wasn't recorded: {e}");
                }
            }
            tracing::info!(
                version,
                bytes,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "store backup: sessions.db backed up before this version's migrations"
            );
            BackupOutcome::Taken { path, bytes }
        }
        Err(e) => {
            tracing::warn!(version, "store backup skipped (will retry next launch): {e}");
            if has_meta {
                let _ = meta_set_conn(conn, META_BACKUP_MISSED, version, 0);
            }
            BackupOutcome::Failed
        }
    }
}

/// The copy itself (see [`backup_before_upgrade`]): the final path and its
/// size. Errors carry no paths or content.
fn take_backup(conn: &Connection, db_path: &std::path::Path, version: &str) -> Result<(PathBuf, u64), String> {
    let (_, stale) = backup_files(db_path);
    for tmp in stale {
        let _ = std::fs::remove_file(tmp);
    }
    let (busy, wal_frames, checkpointed): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| format!("checkpoint: {e}"))?;
    if busy != 0 || wal_frames != checkpointed {
        return Err("checkpoint incomplete: another connection is using the database".into());
    }
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| format!("lock: {e}"))?;
    let copied = (|| {
        let mut wal = db_path.to_path_buf().into_os_string();
        wal.push("-wal");
        if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) > 0 {
            return Err("the WAL grew after the checkpoint".to_string());
        }
        let final_path = backup_path(db_path, version);
        let mut tmp = final_path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = std::fs::copy(db_path, &tmp)
            .and_then(|bytes| std::fs::File::open(&tmp)?.sync_all().map(|_| bytes))
            .and_then(|bytes| std::fs::rename(&tmp, &final_path).map(|_| bytes));
        match result {
            Ok(bytes) => Ok((final_path, bytes)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(format!("copy failed ({:?})", e.kind()))
            }
        }
    })();
    let _ = conn.execute_batch("ROLLBACK");
    copied
}


/// Borrow the shared connection. `pub(crate)` so sibling domains (the planner,
/// `todo::store`) persist through this one connection rather than opening a
/// second writer on the same file.
/// Whether the store has been opened (`init`).
pub(crate) fn is_ready() -> bool {
    CONN.get().is_some()
}

pub(crate) fn with_conn<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
    let conn = CONN.get().ok_or("session store not initialized")?;
    let guard = conn.lock().map_err(|_| "session store lock poisoned")?;
    f(&guard).map_err(|e| e.to_string())
}

// ── One-time migration ──────────────────────────────────────────────────────

/// What the pin rule (`profile_reach::pin_fix`) needs from the settings —
/// the profiles and Hobbes' home — kept current by the app
/// ([`set_pin_rules`]) so the loader and the backfill validate against the
/// profiles as they are now. Process-wide; per thread in tests.
#[cfg(not(test))]
fn pin_rules_cell() -> &'static std::sync::RwLock<Option<crate::settings::Settings>> {
    static CELL: OnceLock<std::sync::RwLock<Option<crate::settings::Settings>>> = OnceLock::new();
    CELL.get_or_init(|| std::sync::RwLock::new(None))
}

#[cfg(test)]
fn pin_rules_cell() -> &'static std::sync::RwLock<Option<crate::settings::Settings>> {
    thread_local! {
        static CELL: &'static std::sync::RwLock<Option<crate::settings::Settings>> =
            Box::leak(Box::new(std::sync::RwLock::new(None)));
    }
    CELL.with(|c| *c)
}

/// Keep the pin rules current: call with the settings whenever they change.
pub fn set_pin_rules(settings: &crate::settings::Settings) {
    let slim = crate::settings::Settings {
        composio_profiles: settings.composio_profiles.clone(),
        home_composio_profile: settings.home_composio_profile.clone(),
        ..Default::default()
    };
    if let Ok(mut r) = pin_rules_cell().write() {
        *r = Some(slim);
    }
}

#[cfg(test)]
fn clear_pin_rules() {
    if let Ok(mut r) = pin_rules_cell().write() {
        *r = None;
    }
}

fn pin_rules() -> Option<crate::settings::Settings> {
    pin_rules_cell().read().ok().and_then(|r| r.clone())
}

/// Ids whose load-time pin was journaled this run: a tab reloaded before
/// its first save isn't journaled twice.
fn load_pins_logged() -> &'static Mutex<std::collections::HashSet<String>> {
    static LOGGED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    LOGGED.get_or_init(Default::default)
}

/// A hydrating ordinary tab gets its pin fixed by the same rule as the
/// backfill (`profile_reach::pin_fix`: its own calls' profile, else
/// Hobbes' home; a legacy name pin becomes its id), so an in-memory copy
/// can never write a missing pin back over a row the backfill pinned.
/// System sessions keep their own rules. Returns whether it changed; the
/// session's fingerprint still matches the old bytes, so its next save
/// writes the pin.
fn pin_on_load(id: &str, session: &mut Session) -> bool {
    if crate::system_sessions::is_system(id) {
        return false;
    }
    let Some(rules) = pin_rules() else { return false };
    match crate::mcp::manager::profile_reach::pin_fix(&rules, session) {
        Some(pin) => {
            session.composio_profile = Some(pin);
            true
        }
        None => false,
    }
}

/// A fork's source pin when the source isn't hydrated: its stored row's,
/// as an id when the rules know it. One indexed row.
pub fn stored_pin(id: &str) -> Option<String> {
    let pin: Option<String> = with_conn(|c| {
        use rusqlite::OptionalExtension;
        c.query_row(
            "SELECT json_extract(data, '$.composio_profile') FROM sessions WHERE id = ?1 AND json_valid(data)",
            params![id],
            |r| r.get(0),
        )
        .optional()
        .map(Option::flatten)
    })
    .ok()
    .flatten();
    let pin = pin.filter(|p| !p.is_empty())?;
    Some(
        pin_rules()
            .and_then(|r| crate::mcp::manager::profile_reach::find_profile(&r, &pin).map(|p| p.id.clone()))
            .unwrap_or(pin),
    )
}

/// Bytes of row data read per pin-backfill batch (at least one row): the
/// connection lock is held only to read a batch and to write its fixes;
/// parsing happens between, without it.
const PIN_BACKFILL_BATCH_BYTES: i64 = 8 * 1024 * 1024;

/// One-time: fix the pin of every stored ordinary tab by the per-tab rule
/// (`profile_reach::pin_fix`) — tabs without a pin get their own calls'
/// profile, else Hobbes' home, else stay unpinned; legacy name pins become
/// ids. A background thread over rowid batches by size; the cursor is kept
/// in meta, so a restart resumes where it stopped.
///
/// Never touches a row that has (or had) an in-memory copy, which is
/// pinned in `SessionState` (startup pass, [`pin_on_load`]) and saved by
/// its own copy: it skips `skip` (the hydrated and system ids at spawn),
/// any system session, any row with a fingerprint (loaded or saved by this
/// process; loads record it under the connection lock), any row written
/// since the spawn (`seq` at or above the high-water mark), and any row
/// whose `seq` changed between the read and the write. A fixed row is
/// rewritten whole (`build_row`) with a fresh `seq`, and its
/// `ComposioProfileSet` is journaled in the same transaction.
pub fn spawn_pin_backfill(skip: std::collections::HashSet<String>) {
    let (done, cursor) = with_conn(|c| Ok((meta_get_conn(c, META_PIN_BACKFILLED)?, meta_get_conn(c, META_PIN_CURSOR)?)))
        .unwrap_or((Some(String::new()), None));
    if done.is_some() {
        return;
    }
    let high_water = SEQ.load(Ordering::SeqCst);
    let mut cursor: i64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let spawned = std::thread::Builder::new().name("pin-backfill".into()).spawn(move || {
        let started = std::time::Instant::now();
        let mut pinned = 0usize;
        loop {
            let Some(rules) = pin_rules() else {
                tracing::warn!("pin backfill: no profile rules; will retry next launch");
                return;
            };
            let step = with_conn(|c| read_pin_batch(c, cursor, PIN_BACKFILL_BATCH_BYTES)).and_then(|batch| match batch {
                None => with_conn(|c| meta_set_conn(c, META_PIN_BACKFILLED, "1", next_seq())).map(|_| None),
                Some((last, rows)) => {
                    let fixes = pin_fixes(&rules, rows, high_water, &skip);
                    with_conn(|c| write_pin_fixes(c, &fixes, last)).map(|n| Some((last, n)))
                }
            });
            match step {
                Ok(Some((last, n))) => {
                    cursor = last;
                    pinned += n;
                }
                Ok(None) => {
                    tracing::info!(
                        pinned,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "pin backfill: stored tabs done"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!("pin backfill failed (will resume next launch): {e}");
                    return;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("pin backfill not started: {e}");
    }
}

/// A stored row as the pin backfill reads it: id, seq, data.
type PinRow = (String, i64, String);

/// The next batch after rowid `after`: rows up to `max_bytes` of data (at
/// least one), and the batch's last rowid. `None` when no rows are left.
fn read_pin_batch(conn: &Connection, after: i64, max_bytes: i64) -> rusqlite::Result<Option<(i64, Vec<PinRow>)>> {
    let mut last = None;
    {
        // `octet_length` reads the size from the record, not the value.
        let mut stmt =
            conn.prepare("SELECT rowid, octet_length(data) FROM sessions WHERE rowid > ?1 ORDER BY rowid LIMIT 256")?;
        let mut rows = stmt.query(params![after])?;
        let mut total = 0i64;
        while let Some(r) = rows.next()? {
            let (rowid, len): (i64, i64) = (r.get(0)?, r.get(1)?);
            if last.is_some() && total + len > max_bytes {
                break;
            }
            total += len;
            last = Some(rowid);
        }
    }
    let Some(last) = last else { return Ok(None) };
    let mut stmt = conn.prepare("SELECT id, seq, data FROM sessions WHERE rowid > ?1 AND rowid <= ?2")?;
    let rows = stmt
        .query_map(params![after, last], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<PinRow>>>()?;
    Ok(Some((last, rows)))
}

/// A fixed row, ready to write: the seq it was read at, the rewritten row
/// and its journal event — all built without the connection lock.
struct PinFix {
    read_seq: i64,
    row: SessionRow,
    events: Vec<SessionEventRow>,
}

/// The rows of a batch whose pin the rule changes, built into the rows to
/// write. Pure apart from the system-session registry; runs without the
/// connection lock. Each row is parsed once for its pin and, only when it
/// is unpinned or holds a profile name, once in full.
fn pin_fixes(
    rules: &crate::settings::Settings,
    rows: Vec<PinRow>,
    high_water: i64,
    skip: &std::collections::HashSet<String>,
) -> Vec<PinFix> {
    #[derive(serde::Deserialize)]
    struct PinProbe {
        #[serde(default)]
        composio_profile: Option<String>,
    }
    rows.into_iter()
        .filter(|(id, seq, _)| *seq < high_water && !skip.contains(id) && !crate::system_sessions::is_system(id))
        .filter_map(|(id, seq, data)| {
            let pin = serde_json::from_str::<PinProbe>(&data).ok()?.composio_profile;
            let candidate = match pin.as_deref() {
                None | Some("") => true,
                Some(p) => crate::mcp::manager::profile_reach::find_profile(rules, p).is_some_and(|f| f.id != p),
            };
            if !candidate {
                return None;
            }
            let mut session = parse_session(&id, &data)?;
            let fixed = crate::mcp::manager::profile_reach::pin_fix(rules, &session)?;
            session.composio_profile = Some(fixed.clone());
            let row = match build_row(&session, next_seq()) {
                Ok(row) => row,
                Err(e) => {
                    tracing::warn!(session_id = %id, "pin backfill: row skipped: {e}");
                    return None;
                }
            };
            let events = prepare_event_rows(&id, &[SessionEvent::ComposioProfileSet { profile: Some(fixed) }]);
            Some(PinFix { read_seq: seq, row, events })
        })
        .collect()
}

/// Write a batch's fixes and advance the cursor to `last`, in one
/// transaction with their journal events. Under the lock only the checks
/// and the writes: a row is written only if it still has the seq it was
/// read at and no in-memory copy (no fingerprint). Returns how many rows
/// were pinned.
fn write_pin_fixes(conn: &Connection, fixes: &[PinFix], last: i64) -> rusqlite::Result<usize> {
    use rusqlite::OptionalExtension;
    let tx = conn.unchecked_transaction()?;
    let mut writes: Vec<&PinFix> = Vec::new();
    for fix in fixes {
        let now: Option<i64> =
            tx.query_row("SELECT seq FROM sessions WHERE id = ?1", params![fix.row.id], |r| r.get(0)).optional()?;
        if now == Some(fix.read_seq) {
            writes.push(fix);
        }
    }
    // Checked under the connection lock (loads record fingerprints under
    // it); the guard is dropped before the journal takes the tombstone
    // lock (`mark_rows_saved` takes tombstones, then fingerprints).
    {
        let fps = fingerprints().lock().unwrap();
        writes.retain(|f| !fps.contains_key(&f.row.id));
    }
    for fix in &writes {
        upsert_row(&tx, &fix.row)?;
        insert_event_rows_in(&tx, &fix.events)?;
    }
    meta_set_conn(&tx, META_PIN_CURSOR, &last.to_string(), next_seq())?;
    tx.commit()?;
    Ok(writes.len())
}

/// One-time: prepend each row's AI topic title to `search_text`, matching what
/// `build_row` now writes, so History search finds chats by the title the
/// drawer shows without parsing every row's `data` per search. Leaves `data`
/// and its hash untouched.
fn backfill_search_tab_titles(conn: &Connection) -> rusqlite::Result<()> {
    if meta_get_conn(conn, META_SEARCH_TAB_TITLE)?.is_some() {
        return Ok(());
    }
    conn.execute(
        "UPDATE sessions
         SET search_text = lower(CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_title') END) || char(10) || search_text
         WHERE trim(coalesce(CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_title') END, '')) <> ''",
        [],
    )?;
    meta_set_conn(conn, META_SEARCH_TAB_TITLE, "1", 0)
}

/// The indexed `project_id` column (a copy of `data.project_id`, written by
/// `upsert_row`). Databases created before it existed get it added here —
/// `ADD COLUMN` is a schema-only change in SQLite, instant at any size —
/// and `backfill_project_id_column` fills it once.
fn ensure_project_id_column(conn: &Connection) -> rusqlite::Result<()> {
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'project_id'")?
        .exists([])?;
    if !has_column {
        conn.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT", [])?;
    }
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_project_id ON sessions(project_id)",
        [],
    )?;
    Ok(())
}

/// One-time: copy each row's `data.project_id` into the `project_id` column,
/// so project lookups (Manage projects' merge, `HOBBES_PROJECT_MERGE`) hit
/// the index instead of scanning every row's JSON. Reads every row once
/// (in `init`, before the connection is shared); writes only the tagged
/// rows. Leaves `data`, `seq` and fingerprints untouched.
fn backfill_project_id_column(conn: &Connection) -> rusqlite::Result<()> {
    if meta_get_conn(conn, META_PROJECT_ID_COLUMN)?.is_some() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let updated = conn.execute(
        "UPDATE sessions
         SET project_id = CASE WHEN json_valid(data) THEN json_extract(data, '$.project_id') END
         WHERE project_id IS NOT CASE WHEN json_valid(data) THEN json_extract(data, '$.project_id') END",
        [],
    )?;
    meta_set_conn(conn, META_PROJECT_ID_COLUMN, "1", 0)?;
    tracing::info!(
        updated,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "backfilled the sessions.project_id column"
    );
    Ok(())
}

/// The `tab_title` column (a copy of `data.tab_title`, written by
/// `upsert_row`), so the Hobbes tab's list query (`tab_rows`) matches titles
/// without parsing every row's JSON under the connection lock. Added to
/// databases created before it (`ADD COLUMN` is schema-only); filled once by
/// `spawn_tab_title_backfill` in the background.
fn ensure_tab_title_column(conn: &Connection) -> rusqlite::Result<()> {
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'tab_title'")?
        .exists([])?;
    if !has_column {
        conn.execute("ALTER TABLE sessions ADD COLUMN tab_title TEXT", [])?;
    }
    Ok(())
}

/// Rows per `tab_title` backfill batch: the connection lock is held for one
/// batch at a time, so saves and History queries interleave with it.
const TAB_TITLE_BACKFILL_BATCH: i64 = 100;

/// One-time: copy each row's `data.tab_title` into the `tab_title` column.
/// Runs on a background thread after `init`, in rowid batches, so a large
/// store (every row's JSON is parsed) never holds the window back: until it
/// finishes, older rows just don't match a title search in `tab_rows`.
/// Interrupted runs resume from the start next launch (each batch is
/// idempotent). Leaves `data`, `seq` and fingerprints untouched.
fn spawn_tab_title_backfill() {
    let done = with_conn(|c| meta_get_conn(c, META_TAB_TITLE_COLUMN)).ok().flatten().is_some();
    if done {
        return;
    }
    let spawned = std::thread::Builder::new().name("tab-title-backfill".into()).spawn(|| {
        let started = std::time::Instant::now();
        let mut cursor = 0i64;
        loop {
            match with_conn(|c| backfill_tab_title_batch(c, cursor, TAB_TITLE_BACKFILL_BATCH)) {
                Ok(Some(next)) => cursor = next,
                Ok(None) => {
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "backfilled the sessions.tab_title column"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!("tab_title column backfill failed (will retry next launch): {e}");
                    return;
                }
            }
            // Let the UI's saves and queries take the lock between batches.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("tab_title column backfill not started: {e}");
    }
}

/// One batch of the `tab_title` backfill: the next `batch` rows after rowid
/// `after`. Returns the cursor for the next batch, or `None` once every row
/// is done (and marks the backfill complete).
fn backfill_tab_title_batch(conn: &Connection, after: i64, batch: i64) -> rusqlite::Result<Option<i64>> {
    let last: Option<i64> = conn.query_row(
        "SELECT max(rowid) FROM (SELECT rowid FROM sessions WHERE rowid > ?1 ORDER BY rowid LIMIT ?2)",
        params![after, batch],
        |r| r.get(0),
    )?;
    let Some(last) = last else {
        meta_set_conn(conn, META_TAB_TITLE_COLUMN, "1", 0)?;
        return Ok(None);
    };
    conn.execute(
        "UPDATE sessions
         SET tab_title = CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_title') END
         WHERE rowid > ?1 AND rowid <= ?2
           AND tab_title IS NOT CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_title') END",
        params![after, last],
    )?;
    Ok(Some(last))
}

/// The whole `tab_title` backfill on one connection (tests).
#[cfg(test)]
fn backfill_tab_title_column(conn: &Connection) -> rusqlite::Result<()> {
    if meta_get_conn(conn, META_TAB_TITLE_COLUMN)?.is_some() {
        return Ok(());
    }
    let mut cursor = 0;
    // A small batch so the tests cross batch boundaries.
    while let Some(next) = backfill_tab_title_batch(conn, cursor, 2)? {
        cursor = next;
    }
    Ok(())
}

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
    /// `session.project_id`, mirrored into an indexed column.
    pub project_id: Option<String>,
    /// `session.tab_title`, mirrored into its own column (`tab_rows`).
    pub tab_title: Option<String>,
    pub seq: i64,
    pub data: String,
    pub data_hash: u64,
    /// Hash of the fields the History drawer lists ([`list_version`]).
    pub list_hash: u64,
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
    // The AI topic title is what the History drawer shows, so search it too.
    if let Some(title) = session.tab_title.as_deref().filter(|t| !t.trim().is_empty()) {
        search_text.push('\n');
        search_text.push_str(&title.to_lowercase());
    }
    search_text.push('\n');
    search_text.push_str(&summary.to_lowercase());
    for m in &session.messages {
        if let Some(t) = m.content.get_text_content() {
            search_text.push('\n');
            search_text.push_str(&t.to_lowercase());
        }
    }
    let list_hash = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            &session.name,
            &session.last_updated,
            session.messages.len(),
            &summary,
            &session.tab_title,
            &session.tab_emoji,
            session.name_user_set,
        )
            .hash(&mut h);
        h.finish()
    };
    Ok(SessionRow {
        list_hash,
        id: session.id.clone(),
        name: session.name.clone(),
        last_updated: fmt_ts(&session.last_updated),
        message_count: session.messages.len() as i64,
        total_cost: session.total_cost(),
        total_tokens: session.total_tokens() as i64,
        // Pending only: startup hydrates these rows, and a session whose
        // timers all fired (a rotated-out system session, say) needn't load.
        has_timers: session.next_pending_timer().is_some(),
        summary,
        search_text,
        project_id: session.project_id.clone(),
        tab_title: session.tab_title.clone(),
        seq,
        data,
        data_hash,
    })
}

fn upsert_row(conn: &Connection, row: &SessionRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sessions
            (id, name, last_updated, message_count, total_cost, total_tokens,
             has_timers, summary, search_text, seq, data, project_id, tab_title)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
             data = excluded.data,
             project_id = excluded.project_id,
             tab_title = excluded.tab_title
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
            row.project_id,
            row.tab_title,
        ],
    )?;
    note_listing(&row.id, row.list_hash);
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

/// Write rows + meta entries atomically. Called on a blocking thread.
/// Meta seqs are assigned at collect time (see `collect_meta_kv`) so an older
/// snapshot whose write lands later cannot overwrite newer meta. Rows whose
/// seq predates a delete tombstone are dropped — they were collected before
/// the session was deleted and must not resurrect it.
pub fn write_rows_and_meta(rows: &[SessionRow], meta: &[(String, String, i64)]) -> Result<(), String> {
    with_conn(|conn| write_rows_and_meta_conn(conn, rows, meta))
}

/// [`write_rows_and_meta`] against a connection: one transaction, so on an
/// error nothing of it lands.
pub(crate) fn write_rows_and_meta_conn(
    conn: &Connection,
    rows: &[SessionRow],
    meta: &[(String, String, i64)],
) -> rusqlite::Result<()> {
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

/// Write one meta key immediately on the calling thread. Used by sibling
/// domains (calendar sync state) for small standalone values that don't ride
/// the session save path.
pub fn meta_set(key: &str, value: &str) -> Result<(), String> {
    let seq = next_seq();
    with_conn(|conn| meta_set_conn(conn, key, value, seq))
}

/// Remove one meta key. Used to retire legacy keys (the pre-per-session
/// global `tool_call_history` blob); missing keys are a no-op.
pub fn meta_delete(key: &str) {
    let _ = with_conn(|conn| {
        conn.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
        Ok(())
    });
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
    let mut cancelled: Vec<(String, Vec<crate::components::chat::Message>)> = Vec::new();
    let result = with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT data FROM sessions WHERE id = ?1")?;
        for id in ids {
            use rusqlite::OptionalExtension;
            let data: Option<String> =
                stmt.query_row(params![id], |r| r.get(0)).optional()?;
            if let Some(data) = data {
                if let Some(mut session) = parse_session(id, &data) {
                    mark_row_loaded(id, &data);
                    // Nothing runs a call across a restart: one still
                    // `Running` on disk was cut off when Hobbes closed. End it
                    // so the activity stream says why, and so the next
                    // approval in this tab doesn't silently re-run it.
                    let ended = session.cancel_running_calls(
                        crate::components::shared::CANCELLED_BY_RESTART,
                        chrono::Utc::now(),
                    );
                    if !ended.is_empty() {
                        cancelled.push((id.clone(), ended));
                    }
                    out.insert(id.clone(), session);
                }
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        tracing::error!("load_sessions failed: {e}");
    }
    // Outside `with_conn`: the system-session check may read meta. A tab
    // the pin migration hasn't reached yet is pinned as it hydrates.
    for (id, session) in out.iter_mut() {
        if pin_on_load(id, session) && load_pins_logged().lock().unwrap().insert(id.clone()) {
            crate::session_events::log_event(
                id,
                SessionEvent::ComposioProfileSet { profile: session.composio_profile.clone() },
            );
        }
    }
    // Journal outside `with_conn`: appending takes the same connection lock.
    for (id, messages) in cancelled {
        tracing::info!(session_id = %id, calls = messages.len(), "Ended tool calls cut off by a restart");
        crate::session_events::log_events(
            &id,
            messages
                .into_iter()
                .map(|message| crate::session_events::SessionEvent::ToolResult { message })
                .collect(),
        );
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

/// The stored size of a session's `data` blob in bytes (what every save of
/// that session rewrites), without reading it. `None` when there is no row.
pub fn row_bytes(id: &str) -> Option<u64> {
    with_conn(|conn| {
        use rusqlite::OptionalExtension;
        conn.query_row(
            // `octet_length` reads the size from the record header; a
            // `CAST` would load the whole value (overflow pages and all).
            "SELECT octet_length(data) FROM sessions WHERE id = ?1",
            params![id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
    })
    .ok()
    .flatten()
    .map(|n| n.max(0) as u64)
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
    forget_listing(id);
    Ok(harvest)
}

/// Rename a session directly in the DB (for sessions not hydrated in memory).
/// Rewrites the `name` field inside the data blob to keep them consistent.
pub fn rename_session(id: &str, new_name: &str) {
    if let Err(e) = with_conn(|conn| rename_session_conn(conn, id, new_name)) {
        tracing::error!("rename_session({id}) failed: {e}");
    }
}

fn rename_session_conn(conn: &Connection, id: &str, new_name: &str) -> rusqlite::Result<()> {
    {
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
        forget_listing(id);
        Ok(())
    }
}

/// Ids of rows tagged to any of `project_ids` — an index lookup on the
/// `project_id` column (the session's own tag; tool-call arguments inside
/// `data` that happen to carry `project_id` keys never match).
fn tagged_ids_conn(conn: &Connection, project_ids: &[String]) -> rusqlite::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached("SELECT id FROM sessions WHERE project_id = ?1")?;
    for pid in project_ids {
        let rows = stmt.query_map(params![pid], |r| r.get::<_, String>(0))?;
        for id in rows {
            let id = id?;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// Rows tagged to any of `project_ids`, except `skip_ids`: `(id, data as
/// JSON, project_tag_user_set)`. Selected by the indexed column; `data` is
/// read only for those rows.
fn tagged_rows_conn(
    conn: &Connection,
    project_ids: &[String],
    skip_ids: &std::collections::HashSet<String>,
) -> rusqlite::Result<Vec<(String, serde_json::Value, bool)>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached("SELECT data FROM sessions WHERE id = ?1")?;
    for id in tagged_ids_conn(conn, project_ids)? {
        if skip_ids.contains(&id) {
            continue;
        }
        let data: String = match stmt.query_row(params![id], |r| r.get(0)) {
            Ok(d) => d,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(e),
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        let user_set = value
            .get("project_tag_user_set")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        out.push((id, value, user_set));
    }
    Ok(out)
}

/// Ids of stored sessions tagged to any of `project_ids` (hydrated or not —
/// a hydrated session's row may lag its in-memory copy; callers that care
/// overlay `SessionState`). An index lookup, cheap at any database size.
pub fn session_ids_tagged(project_ids: &[String]) -> Vec<String> {
    with_conn(|conn| tagged_ids_conn(conn, project_ids)).unwrap_or_default()
}

/// Retag every stored session tagged to one of `from_ids` to `to_id`,
/// skipping `skip_ids` (the hydrated sessions — the caller retags those in
/// `SessionState`, whose next save writes their rows). Rewrites the `data`
/// blob in place with a fresh `seq` (so an older in-flight write can't
/// clobber it) and drops any stale fingerprint. Returns `(session_id,
/// project_tag_user_set)` per retagged row, for journaling.
pub fn retag_sessions(
    from_ids: &[String],
    to_id: &str,
    skip_ids: &std::collections::HashSet<String>,
) -> Vec<(String, bool)> {
    match with_conn(|conn| retag_sessions_conn(conn, from_ids, to_id, skip_ids)) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("retag_sessions failed: {e}");
            Vec::new()
        }
    }
}

fn retag_sessions_conn(
    conn: &Connection,
    from_ids: &[String],
    to_id: &str,
    skip_ids: &std::collections::HashSet<String>,
) -> rusqlite::Result<Vec<(String, bool)>> {
    let rows = tagged_rows_conn(conn, from_ids, skip_ids)?;
    let tx = conn.unchecked_transaction()?;
    let mut done = Vec::new();
    for (id, mut value, user_set) in rows {
        value["project_id"] = serde_json::Value::String(to_id.to_string());
        let Ok(new_data) = serde_json::to_string(&value) else { continue };
        // The column moves with `data` so later lookups stay exact.
        tx.execute(
            "UPDATE sessions SET data = ?2, project_id = ?3, seq = ?4 WHERE id = ?1",
            params![id, new_data, to_id, next_seq()],
        )?;
        forget_fingerprint(&id);
        done.push((id, user_set));
    }
    tx.commit()?;
    Ok(done)
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

#[derive(Clone, Debug, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub last_updated: String,
    pub message_count: i64,
    pub summary: String,
    /// Tab identity: `tab_title` from its column (or the row's JSON while
    /// the column backfill hasn't reached it), `tab_emoji`/`name_user_set`
    /// via `json_extract` ([`META_SELECT`]).
    pub tab_title: Option<String>,
    pub tab_emoji: Option<String>,
    pub name_user_set: bool,
}

impl SessionMeta {
    /// Mirrors `Session::display_title` for a non-hydrated row.
    pub fn display_title(&self) -> String {
        if self.name_user_set {
            self.name.clone()
        } else if let Some(title) = &self.tab_title {
            title.clone()
        } else {
            self.name.clone()
        }
    }

    /// The emoji to show in front of the title, if the chat has a real one.
    pub fn display_emoji(&self) -> Option<&str> {
        self.tab_emoji.as_deref().map(str::trim).filter(|e| !e.is_empty())
    }

    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            name: r.get(1)?,
            last_updated: r.get(2)?,
            message_count: r.get(3)?,
            summary: r.get(4)?,
            tab_title: r.get(5)?,
            tab_emoji: r.get(6)?,
            name_user_set: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
        })
    }
}

/// The [`SessionMeta`] columns. Only the listed fields are read: no row's
/// `data` is deserialized. `tab_title` comes from its column, falling back
/// to the JSON while `spawn_tab_title_backfill` hasn't filled it; the two
/// JSON-only fields use `json_extract` (SQLite parses each row's JSON once
/// and shares it between the calls). `json_valid` keeps one malformed row
/// from failing the whole listing.
const META_SELECT: &str = "id, name, last_updated, message_count, summary, \
     coalesce(tab_title, CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_title') END), \
     CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_emoji') END, \
     CASE WHEN json_valid(data) THEN json_extract(data, '$.name_user_set') END";

/// Paginated, newest-first metadata listing. `query` (lowercased substring)
/// matches name, summary, or message text. Returns (page, total_matches).
pub fn list_metadata(query: Option<&str>, offset: i64, limit: i64) -> (Vec<SessionMeta>, i64) {
    let result = with_conn(|conn| list_metadata_conn(conn, query, offset, limit));
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("list_metadata failed: {e}");
            (Vec::new(), 0)
        }
    }
}

/// `list_metadata` against an explicit connection (so tests can use an
/// in-memory DB). The query matches name, summary and `search_text` (which
/// carries the AI topic title and message text, `build_row`); all columns,
/// so filtering parses no JSON.
fn list_metadata_conn(
    conn: &Connection,
    query: Option<&str>,
    offset: i64,
    limit: i64,
) -> rusqlite::Result<(Vec<SessionMeta>, i64)> {
    {
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
            "SELECT {META_SELECT} FROM sessions {where_clause} \
             ORDER BY last_updated DESC LIMIT {limit} OFFSET {offset}"
        );

        let map_row = SessionMeta::from_row;

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

/// [`SessionMeta`] for `ids`, in that order (missing ids skipped).
pub fn metadata_by_ids(ids: &[String]) -> Vec<SessionMeta> {
    with_conn(|conn| metadata_by_ids_conn(conn, ids)).unwrap_or_else(|e| {
        tracing::error!("metadata_by_ids failed: {e}");
        Vec::new()
    })
}

fn metadata_by_ids_conn(conn: &Connection, ids: &[String]) -> rusqlite::Result<Vec<SessionMeta>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn.prepare(&format!("SELECT {META_SELECT} FROM sessions WHERE id = ?1"))?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(m) = stmt.query_row(params![id], SessionMeta::from_row).optional()? {
            out.push(m);
        }
    }
    Ok(out)
}

/// A conversation row for the Hobbes tab's `HOBBES_TAB_LIST` (`hobbes_tabs`):
/// metadata only, read with `json_extract` so no message blob is parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct TabRow {
    pub id: String,
    pub name: String,
    pub tab_title: Option<String>,
    pub tab_emoji: Option<String>,
    pub name_user_set: bool,
    pub project_id: Option<String>,
    pub composio_profile: Option<String>,
    /// RFC 3339 (the `last_updated` column).
    pub last_updated: String,
    pub message_count: i64,
}

impl TabRow {
    /// Mirrors `Session::display_title` for a non-hydrated row.
    pub fn display_title(&self) -> String {
        match (&self.tab_title, self.name_user_set) {
            (Some(t), false) => t.clone(),
            _ => self.name.clone(),
        }
    }
}

/// Whether a conversation's name or AI title contains `query_lower` (already
/// lowercased), folding case with Unicode rules (`str::to_lowercase`). The
/// one matcher for the Hobbes tab's listing, in memory
/// (`hobbes_tabs::session_matches`) and from the store ([`tab_rows`]), so a
/// hydrated tab and a stored one match the same queries.
pub fn title_matches(name: &str, tab_title: Option<&str>, query_lower: &str) -> bool {
    let has = |t: &str| t.to_lowercase().contains(query_lower);
    has(name) || tab_title.is_some_and(has)
}

/// Newest-first conversation rows, skipping `exclude` (the Hobbes lineage
/// and the hydrated sessions the caller lists from memory). `query`
/// matches the name and the AI title only ([`title_matches`]); `project_id`
/// filters on the indexed column.
///
/// Two steps, so no row's JSON is parsed to filter: the `last_updated`
/// index walk reads only the `id`, `name` and `tab_title` columns and
/// matches the query in Rust (SQLite's `LIKE` folds ASCII case only), and
/// stops once `limit` rows are kept; each match's JSON-only fields are then
/// read by id. A row whose `data` isn't valid JSON is skipped (it couldn't
/// be read with `HOBBES_TAB_READ` either) and doesn't fail the listing.
pub fn tab_rows(query: Option<&str>, project_id: Option<&str>, exclude: &[String], limit: i64) -> Vec<TabRow> {
    with_conn(|conn| tab_rows_conn(conn, query, project_id, exclude, limit)).unwrap_or_else(|e| {
        tracing::error!("tab_rows failed: {e}");
        Vec::new()
    })
}

pub(crate) fn tab_rows_conn(
    conn: &Connection,
    query: Option<&str>,
    project_id: Option<&str>,
    exclude: &[String],
    limit: i64,
) -> rusqlite::Result<Vec<TabRow>> {
    use rusqlite::OptionalExtension;
    let limit = limit.max(0) as usize;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let query = query.map(str::trim).filter(|q| !q.is_empty()).map(str::to_lowercase);
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(p) = project_id {
        values.push(p.to_string().into());
        clauses.push(format!("project_id = ?{}", values.len()));
    }
    if !exclude.is_empty() {
        let start = values.len();
        let marks: Vec<String> = (0..exclude.len()).map(|i| format!("?{}", start + i + 1)).collect();
        values.extend(exclude.iter().map(|id| rusqlite::types::Value::from(id.clone())));
        clauses.push(format!("id NOT IN ({})", marks.join(", ")));
    }
    let where_clause = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
    // Step 1: columns only. No SQL LIMIT: the walk stops in Rust once
    // `limit` rows are kept, which a skipped (malformed) row doesn't count
    // towards.
    let mut walk = conn.prepare(&format!(
        "SELECT id, name, tab_title, project_id, last_updated, message_count          FROM sessions {where_clause} ORDER BY last_updated DESC"
    ))?;
    // Step 2: the JSON-only fields of one kept row; `json_valid` first, so
    // a malformed row reads as NULL instead of failing the statement.
    let mut fields = conn.prepare(
        "SELECT json_valid(data),          CASE WHEN json_valid(data) THEN json_extract(data, '$.tab_emoji') END,          CASE WHEN json_valid(data) THEN coalesce(json_extract(data, '$.name_user_set'), 0) END,          CASE WHEN json_valid(data) THEN json_extract(data, '$.composio_profile') END          FROM sessions WHERE id = ?1",
    )?;
    let mut rows = walk.query(rusqlite::params_from_iter(values))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let (id, name, tab_title): (String, String, Option<String>) = (r.get(0)?, r.get(1)?, r.get(2)?);
        if query.as_deref().is_some_and(|q| !title_matches(&name, tab_title.as_deref(), q)) {
            continue;
        }
        type Fields = (bool, Option<String>, Option<i64>, Option<String>);
        let Some((valid, tab_emoji, name_user_set, composio_profile)) = fields
            .query_row(params![id], |f| Ok::<Fields, _>((f.get(0)?, f.get(1)?, f.get(2)?, f.get(3)?)))
            .optional()?
        else {
            continue;
        };
        if !valid {
            tracing::warn!(session_id = %id, "tab_rows: skipped a row whose data isn't valid JSON");
            continue;
        }
        out.push(TabRow {
            id,
            name,
            tab_title,
            tab_emoji,
            name_user_set: name_user_set.unwrap_or(0) != 0,
            project_id: r.get(3)?,
            composio_profile,
            last_updated: r.get(4)?,
            message_count: r.get(5)?,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Read one session for viewing only (`HOBBES_TAB_READ`, the Heartbeat's
/// worker diagnoses): parsed from its row, with none of [`load_sessions`]'
/// hydration side effects — no fingerprint recorded, no cut-off call ended
/// or journalled. The result must never be put into `SessionState`.
pub fn peek_session(id: &str) -> Option<Session> {
    // Only the fetch holds the connection lock; the parse runs after.
    let data = with_conn(|conn| peek_data_conn(conn, id)).ok().flatten()?;
    parse_session(id, &data)
}

fn peek_data_conn(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT data FROM sessions WHERE id = ?1", params![id], |r| r.get(0)).optional()
}

#[cfg(test)]
pub(crate) fn peek_session_conn(conn: &Connection, id: &str) -> rusqlite::Result<Option<Session>> {
    Ok(peek_data_conn(conn, id)?.and_then(|d| parse_session(id, &d)))
}

/// `meta` key set once system sessions' legacy summary sources are cleared
/// ([`clear_system_summary_sources_once`]).
pub const SYSTEM_SUMMARY_SOURCES_CLEARED_KEY: &str = "system_summary_sources_cleared";

/// One-time, at startup before any hydration: clear `summary_sources` on
/// the system sessions `ids` (the Hobbes lineage). Before tab reads, a
/// system session's summary recorded every Composio read, which the
/// cross-company hold ignored there; it now counts a system session's
/// `summary_sources` with no clock (they hold only tab-read sources from
/// here on, `profile_reach::durable_summary_sources`), so the old entries
/// must go rather than hold writes for good. Guarded by
/// [`SYSTEM_SUMMARY_SOURCES_CLEARED_KEY`], written in the same transaction
/// as the rows; each clear is journaled as a `SummaryComputed` without
/// sources, so a replay matches.
pub fn clear_system_summary_sources_once(ids: &[String]) {
    match with_conn(|conn| clear_summary_sources_conn(conn, ids)) {
        Ok(cleared) => {
            if !cleared.is_empty() {
                tracing::info!(sessions = cleared.len(), "Cleared legacy summary sources on system sessions");
            }
            for (id, summary) in cleared {
                crate::session_events::log_event(&id, crate::session_events::SessionEvent::SummaryComputed { summary });
            }
        }
        Err(e) => tracing::error!("clearing system summary sources failed (will retry next launch): {e}"),
    }
}

/// [`clear_system_summary_sources_once`] against a connection: the cleared
/// sessions with the `SummaryComputed` payload to journal for each.
pub(crate) fn clear_summary_sources_conn(
    conn: &Connection,
    ids: &[String],
) -> rusqlite::Result<Vec<(String, serde_json::Value)>> {
    if meta_get_conn(conn, SYSTEM_SUMMARY_SOURCES_CLEARED_KEY)?.is_some() {
        return Ok(Vec::new());
    }
    let tx = conn.unchecked_transaction()?;
    let mut out = Vec::new();
    for id in ids {
        let Some(mut session) = peek_data_conn(&tx, id)?.and_then(|d| parse_session(id, &d)) else { continue };
        if session.summary_sources.is_empty() {
            continue;
        }
        session.summary_sources.clear();
        let row = build_row(&session, next_seq()).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        upsert_row(&tx, &row)?;
        out.push((id.clone(), crate::session::summary_payload(&session.active_context.conversation_summary, &[])));
    }
    meta_set_conn(&tx, SYSTEM_SUMMARY_SOURCES_CLEARED_KEY, "1", next_seq())?;
    tx.commit()?;
    Ok(out)
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
    insert_event_rows_in(&tx, rows)?;
    tx.commit()
}

/// [`insert_event_rows_conn`] inside a transaction the caller holds.
fn insert_event_rows_in(tx: &Connection, rows: &[SessionEventRow]) -> rusqlite::Result<()> {
    {
        // A tombstoned session's journal must not grow again: a stream that
        // is still running when its tab is deleted keeps producing events
        // (StreamCancelled, tool results, ...) after `delete_session` already
        // dropped the row and its journal.
        //
        // RESURRECTION ASYMMETRY (deliberate). `write_rows_and_meta` drops
        // only rows collected *before* the tombstone and treats a newer row
        // as a re-create (clearing the tombstone). Event rows do carry a seq
        // from the same counter, so the same seq comparison is available —
        // but it is the wrong rule here, because the post-delete events this
        // guard exists to stop are exactly the ones with a seq *newer* than
        // the tombstone. An event is never itself evidence of a re-create,
        // so while the tombstone stands every event for that id is dropped
        // regardless of seq. Nothing is lost forever: a genuine re-create of
        // the id writes a session row, and that write clears the tombstone,
        // after which the journal accepts events again.
        let tombs = tombstones().lock().unwrap();
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO session_events (session_id, seq, ts, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for row in rows {
            if tombs.contains_key(&row.session_id) {
                continue;
            }
            stmt.execute(params![row.session_id, row.seq, row.ts, row.kind, row.payload])?;
        }
    }
    Ok(())
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
    ///
    /// The tombstone map is process-wide, so a test that deletes a session
    /// would otherwise leak its tombstone into every later test's writes
    /// (silently dropping their rows and events). Clear it per test.
    pub fn with_test_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
        clear_tombstones();
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        f(&conn)
    }

    /// Drop every delete tombstone. Tests only — see `with_test_db`.
    pub fn clear_tombstones() {
        tombstones().lock().unwrap().clear();
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
        // Mirror the public `delete_session()` exactly: forget fingerprint,
        // insert tombstone, then delete the row and events.
        forget_fingerprint(id);
        tombstones().lock().unwrap().insert(id.to_string(), next_seq());
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

    pub fn session_ids_tagged_for_test(conn: &Connection, project_ids: &[String]) -> Vec<String> {
        tagged_ids_conn(conn, project_ids).unwrap()
    }

    pub fn project_id_column(conn: &Connection, id: &str) -> Option<String> {
        conn.query_row("SELECT project_id FROM sessions WHERE id = ?1", params![id], |r| r.get(0))
            .unwrap()
    }

    pub fn backfill_project_ids_for_test(conn: &Connection) {
        backfill_project_id_column(conn).unwrap();
    }

    pub fn backfill_tab_titles(conn: &Connection) {
        conn.execute("DELETE FROM meta WHERE key = ?1", params![META_TAB_TITLE_COLUMN]).unwrap();
        backfill_tab_title_column(conn).unwrap();
    }

    pub fn ensure_project_id_column_for_test(conn: &Connection) {
        ensure_project_id_column(conn).unwrap();
    }

    pub fn retag_sessions_for_test(
        conn: &Connection,
        from_ids: &[String],
        to_id: &str,
        skip_ids: &std::collections::HashSet<String>,
    ) -> Vec<(String, bool)> {
        retag_sessions_conn(conn, from_ids, to_id, skip_ids).unwrap()
    }
}

#[cfg(test)]
mod list_metadata_tests {
    use super::test_support::*;
    use super::*;

    fn session(name: &str, user_set: bool, title: Option<&str>, emoji: Option<&str>) -> Session {
        let mut state = crate::session::SessionState::default();
        let id = state.create_session_raw(None);
        let mut s = state.sessions.remove(&id).unwrap();
        s.name = name.to_string();
        s.name_user_set = user_set;
        s.tab_title = title.map(str::to_string);
        s.tab_emoji = emoji.map(str::to_string);
        s
    }

    #[test]
    fn retag_rewrites_only_non_hydrated_rows_tagged_to_merged_projects() {
        with_test_db(|conn| {
            let mut a = session("A", false, None, None);
            a.project_id = Some("old-1".into());
            let mut b = session("B", false, None, None);
            b.project_id = Some("old-2".into());
            b.project_tag_user_set = true;
            let mut hydrated = session("H", false, None, None);
            hydrated.project_id = Some("old-1".into());
            let mut other = session("O", false, None, None);
            other.project_id = Some("keep-me".into());
            for s in [&a, &b, &hydrated, &other] {
                upsert(conn, s);
            }
            let from = vec!["old-1".to_string(), "old-2".to_string()];
            let mut tagged = session_ids_tagged_for_test(conn, &from);
            tagged.sort();
            let mut want = vec![a.id.clone(), b.id.clone(), hydrated.id.clone()];
            want.sort();
            assert_eq!(tagged, want, "only the sessions' own tags count");

            let seq_before: i64 = conn
                .query_row("SELECT seq FROM sessions WHERE id = ?1", params![a.id], |r| r.get(0))
                .unwrap();
            let skip: std::collections::HashSet<String> = [hydrated.id.clone()].into();
            let mut done = retag_sessions_for_test(conn, &from, "keep", &skip);
            done.sort();
            let mut want = vec![(a.id.clone(), false), (b.id.clone(), true)];
            want.sort();
            assert_eq!(done, want);

            let tag_of = |id: &str| -> Option<String> {
                let data = get_row_data(conn, id).unwrap();
                serde_json::from_str::<Session>(&data).unwrap().project_id
            };
            assert_eq!(tag_of(&a.id).as_deref(), Some("keep"));
            assert_eq!(tag_of(&b.id).as_deref(), Some("keep"));
            assert_eq!(tag_of(&hydrated.id).as_deref(), Some("old-1"), "hydrated rows are the caller's");
            assert_eq!(tag_of(&other.id).as_deref(), Some("keep-me"));
            let seq_after: i64 = conn
                .query_row("SELECT seq FROM sessions WHERE id = ?1", params![a.id], |r| r.get(0))
                .unwrap();
            assert!(seq_after > seq_before, "a fresh seq keeps older in-flight writes out");
            // The user_set flag rides along unchanged.
            let b_row: Session = serde_json::from_str(&get_row_data(conn, &b.id).unwrap()).unwrap();
            assert!(b_row.project_tag_user_set);
            // The indexed column moved with `data`.
            assert_eq!(project_id_column(conn, &a.id).as_deref(), Some("keep"));
            assert_eq!(project_id_column(conn, &hydrated.id).as_deref(), Some("old-1"));
            let mut now_keep = session_ids_tagged_for_test(conn, &["keep".to_string()]);
            now_keep.sort();
            let mut want = vec![a.id.clone(), b.id.clone()];
            want.sort();
            assert_eq!(now_keep, want);
            assert!(session_ids_tagged_for_test(conn, &["old-2".to_string()]).is_empty());
        });
    }

    #[test]
    fn upsert_writes_the_project_id_column_and_tool_args_never_match() {
        with_test_db(|conn| {
            let mut a = session("A", false, None, None);
            a.project_id = Some("p1".into());
            // A chat that only *mentions* p1 in a tool call's arguments.
            let mut mention = session("M", false, None, None);
            let tc = crate::components::shared::ToolCall::new(
                "hobbes-planner".into(),
                "HOBBES_TODO_CREATE".into(),
                serde_json::json!({ "todos": [{ "title": "x", "project_id": "p1" }] }),
                None,
                None,
            );
            mention.messages.push(crate::components::chat::Message {
                id: uuid::Uuid::new_v4(),
                author: "Hobbes".into(),
                content: crate::components::shared::MessageContent::ToolCall(tc),
                attachments: Vec::new(),
                comments: Vec::new(),
                created_at: chrono::Utc::now(),
                usage: None,
            });
            upsert(conn, &a);
            upsert(conn, &mention);
            assert_eq!(project_id_column(conn, &a.id).as_deref(), Some("p1"));
            assert_eq!(project_id_column(conn, &mention.id), None);
            assert_eq!(session_ids_tagged_for_test(conn, &["p1".to_string()]), vec![a.id.clone()]);

            // A later save that clears the tag clears the column.
            a.project_id = None;
            upsert(conn, &a);
            assert_eq!(project_id_column(conn, &a.id), None);
            assert!(session_ids_tagged_for_test(conn, &["p1".to_string()]).is_empty());

            // An older in-flight write (lower seq) can't put the column back.
            let mut stale = a.clone();
            stale.project_id = Some("p1".into());
            upsert_with_seq(conn, &stale, 0);
            assert_eq!(project_id_column(conn, &a.id), None);
        });
    }

    #[test]
    fn project_id_column_migration_adds_backfills_once_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        // A pre-column database: the old sessions table, rows carrying the
        // tag only inside `data`.
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, name TEXT NOT NULL, last_updated TEXT NOT NULL,
                 message_count INTEGER NOT NULL, total_cost REAL NOT NULL,
                 total_tokens INTEGER NOT NULL, has_timers INTEGER NOT NULL,
                 summary TEXT NOT NULL, search_text TEXT NOT NULL, seq INTEGER NOT NULL,
                 data TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL, seq INTEGER NOT NULL);",
        )
        .unwrap();
        let mut tagged = session("T", false, None, None);
        tagged.project_id = Some("p1".into());
        let untagged = session("U", false, None, None);
        for s in [&tagged, &untagged] {
            let row = build_row(s, 5).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, name, last_updated, message_count, total_cost,
                     total_tokens, has_timers, summary, search_text, seq, data)
                 VALUES (?1, ?2, ?3, 0, 0, 0, 0, '', '', ?4, ?5)",
                params![row.id, row.name, row.last_updated, row.seq, row.data],
            )
            .unwrap();
        }

        ensure_project_id_column_for_test(&conn);
        // Idempotent: a second schema pass doesn't try to add it again.
        ensure_project_id_column_for_test(&conn);
        assert_eq!(project_id_column(&conn, &tagged.id), None, "not backfilled yet");

        backfill_project_ids_for_test(&conn);
        assert_eq!(project_id_column(&conn, &tagged.id).as_deref(), Some("p1"));
        assert_eq!(project_id_column(&conn, &untagged.id), None);
        assert_eq!(meta(&conn, META_PROJECT_ID_COLUMN).as_deref(), Some("1"));
        // `data` and `seq` are untouched (fingerprints stay valid).
        let (seq, data): (i64, String) = conn
            .query_row("SELECT seq, data FROM sessions WHERE id = ?1", params![tagged.id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(seq, 5);
        assert_eq!(data, build_row(&tagged, 5).unwrap().data);

        // Guarded by the meta key: a second run is a no-op.
        conn.execute("UPDATE sessions SET project_id = NULL", []).unwrap();
        backfill_project_ids_for_test(&conn);
        assert_eq!(project_id_column(&conn, &tagged.id), None);

        // The lookup uses the index, not a scan.
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM sessions WHERE project_id = 'p1'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("idx_sessions_project_id"), "{plan}");
    }

    /// `seed_seq_from_db` runs before the window shows: its `MAX(seq)`
    /// reads must be index lookups, never a walk of every session's row.
    #[test]
    fn seq_high_water_reads_use_an_index() {
        with_test_db(|conn| {
            // session_events' MAX(seq) scans its (session_id, seq) key
            // index, a few MB: cheaper than a one-time index build over
            // the journal's hundreds of MB.
            for (table, index) in [("sessions", "idx_sessions_seq"), ("fleet_events", "idx_fleet_events_seq")] {
                let plan: String = conn
                    .query_row(&format!("EXPLAIN QUERY PLAN SELECT MAX(seq) FROM {table}"), [], |r| r.get(3))
                    .unwrap();
                assert!(plan.contains(index), "{table}: {plan}");
            }
        });
    }

    /// The listing reads only the listed fields: a row whose `data` is huge
    /// and isn't even a `Session` still lists with the right values.
    #[test]
    fn listing_reads_fields_without_deserializing_data() {
        with_test_db(|conn| {
            let s = session("Sep 18 - 10:00 AM", true, Some("Trip Planning"), Some("🧳"));
            upsert(conn, &s);
            let blob = "x".repeat(4 * 1024 * 1024);
            let data = serde_json::json!({
                "tab_title": "Trip Planning",
                "tab_emoji": "🧳",
                "name_user_set": true,
                "messages": [{ "huge": blob }],
            })
            .to_string();
            conn.execute("UPDATE sessions SET data = ?2 WHERE id = ?1", params![s.id, data]).unwrap();
            assert!(parse_session(&s.id, &data).is_none(), "data must not be a Session for this test");

            let (metas, total) = list_metadata_conn(conn, None, 0, 10).unwrap();
            assert_eq!(total, 1);
            let m = &metas[0];
            assert_eq!(m.id, s.id);
            assert_eq!(m.name, "Sep 18 - 10:00 AM");
            assert_eq!(m.tab_title.as_deref(), Some("Trip Planning"));
            assert_eq!(m.display_emoji(), Some("🧳"));
            assert!(m.name_user_set);
            assert_eq!(m.display_title(), "Sep 18 - 10:00 AM");
            assert_eq!(metadata_by_ids_conn(conn, std::slice::from_ref(&s.id)).unwrap()[0], *m);
        });
    }

    #[test]
    fn listing_survives_malformed_data() {
        with_test_db(|conn| {
            let s = session("Sep 19 - 08:00 AM", false, Some("Garden"), Some("🌱"));
            upsert(conn, &s);
            conn.execute("UPDATE sessions SET data = '{not json' WHERE id = ?1", params![s.id]).unwrap();
            let (metas, _) = list_metadata_conn(conn, None, 0, 10).unwrap();
            // The column still carries the title; the JSON-only fields are blank.
            assert_eq!(metas[0].display_title(), "Garden");
            assert_eq!(metas[0].tab_emoji, None);
            assert!(!metas[0].name_user_set);
        });
    }

    /// Until `spawn_tab_title_backfill` reaches a row, its `tab_title`
    /// column is NULL: the title comes from the JSON instead.
    #[test]
    fn null_tab_title_column_falls_back_to_json() {
        with_test_db(|conn| {
            let titled = session("Sep 20 - 09:00 AM", false, Some("Quarterly Budget Review"), Some("💰"));
            let untitled = session("Sep 21 - 09:00 AM", false, None, None);
            upsert(conn, &titled);
            upsert(conn, &untitled);
            conn.execute("UPDATE sessions SET tab_title = NULL", []).unwrap();
            let metas = metadata_by_ids_conn(conn, &[titled.id.clone(), untitled.id.clone()]).unwrap();
            assert_eq!(metas[0].display_title(), "Quarterly Budget Review");
            assert_eq!(metas[1].tab_title, None);
            assert_eq!(metas[1].display_title(), "Sep 21 - 09:00 AM");
        });
    }

    /// One row whose `data` isn't JSON: the column backfills skip it and
    /// still finish (their done flags set), instead of failing every launch.
    #[test]
    fn a_malformed_row_never_stops_the_column_backfills() {
        with_test_db(|conn| {
            let a = session("Sep 25 - 09:00 AM", false, Some("Garden"), None);
            let bad = session("Sep 25 - 10:00 AM", false, Some("Broken"), None);
            let c = session("Sep 25 - 11:00 AM", false, Some("Taxes"), None);
            for s in [&a, &bad, &c] {
                upsert(conn, s);
            }
            conn.execute("UPDATE sessions SET data = '{not json' WHERE id = ?1", params![bad.id]).unwrap();
            conn.execute("UPDATE sessions SET tab_title = NULL, project_id = 'stale'", []).unwrap();

            test_support::backfill_tab_titles(conn);
            assert!(meta(conn, META_TAB_TITLE_COLUMN).is_some(), "the backfill finished");
            let title = |id: &str| -> Option<String> {
                conn.query_row("SELECT tab_title FROM sessions WHERE id = ?1", params![id], |r| r.get(0)).unwrap()
            };
            assert_eq!(title(&a.id).as_deref(), Some("Garden"));
            assert_eq!(title(&bad.id), None, "skipped");
            assert_eq!(title(&c.id).as_deref(), Some("Taxes"), "rows after it are still filled");

            conn.execute("DELETE FROM meta WHERE key IN (?1, ?2)", params![META_PROJECT_ID_COLUMN, META_SEARCH_TAB_TITLE])
                .unwrap();
            backfill_project_id_column(conn).unwrap();
            backfill_search_tab_titles(conn).unwrap();
            assert!(meta(conn, META_PROJECT_ID_COLUMN).is_some() && meta(conn, META_SEARCH_TAB_TITLE).is_some());
        });
    }

    /// `HOBBES_TAB_LIST`'s store rows: a malformed row is skipped, not the
    /// whole listing, and doesn't count towards the limit.
    #[test]
    fn tab_rows_skip_a_malformed_row() {
        with_test_db(|conn| {
            let now = chrono::Utc::now();
            let mut rows = Vec::new();
            for (i, title) in ["Oldest", "Middle", "Broken newest"].into_iter().enumerate() {
                let mut s = session(&format!("Sep 26 - 0{i}:00 AM"), false, Some(title), Some("🧾"));
                s.last_updated = now - chrono::Duration::minutes(10 - i as i64);
                upsert(conn, &s);
                rows.push(s);
            }
            conn.execute("UPDATE sessions SET data = '{not json' WHERE id = ?1", params![rows[2].id]).unwrap();
            let listed = tab_rows_conn(conn, None, None, &[], 2).unwrap();
            let titles: Vec<String> = listed.iter().map(TabRow::display_title).collect();
            assert_eq!(titles, ["Middle", "Oldest"]);
            assert_eq!(listed[0].tab_emoji.as_deref(), Some("🧾"));
            assert!(tab_rows_conn(conn, Some("broken"), None, &[], 10).unwrap().is_empty());
        });
    }

    /// The store's title search folds case like the in-memory one
    /// (`str::to_lowercase`), not just ASCII the way SQLite's `LIKE` does.
    #[test]
    fn tab_rows_fold_non_ascii_case_like_memory() {
        with_test_db(|conn| {
            let titled = session("Sep 27 - 09:00 AM", false, Some("Ärger mit Behörden"), None);
            let named = session("ÉTÉ À PARIS", true, None, None);
            upsert(conn, &titled);
            upsert(conn, &named);
            let hits = |q: &str| tab_rows_conn(conn, Some(q), None, &[], 10).unwrap().len();
            assert_eq!(hits("ärger"), 1);
            assert_eq!(hits("ÄRGER"), 1);
            assert_eq!(hits("été"), 1);
            assert_eq!(hits("100%"), 0, "no LIKE wildcards");
            assert!(title_matches("ÉTÉ À PARIS", None, "été à"));
            assert!(!title_matches("Garden", Some("Taxes"), "zz"));
        });
    }

    #[test]
    fn row_bytes_reads_the_stored_size() {
        with_test_db(|conn| {
            let s = session("Sep 28 - 09:00 AM", false, None, None);
            upsert(conn, &s);
            let n: i64 = conn
                .query_row("SELECT octet_length(data) FROM sessions WHERE id = ?1", params![s.id], |r| r.get(0))
                .unwrap();
            assert_eq!(n as usize, get_row_data(conn, &s.id).unwrap().len());
        });
    }

    #[test]
    fn search_matches_name_and_title_and_misses_the_rest() {
        with_test_db(|conn| {
            upsert(conn, &session("Garden notes", false, None, None));
            upsert(conn, &session("Sep 22 - 09:00 AM", false, Some("Garden Planning"), None));
            upsert(conn, &session("Sep 23 - 09:00 AM", false, Some("Taxes"), None));
            let (metas, total) = list_metadata_conn(conn, Some("GARDEN"), 0, 10).unwrap();
            assert_eq!(total, 2);
            let mut titles: Vec<String> = metas.iter().map(SessionMeta::display_title).collect();
            titles.sort();
            assert_eq!(titles, ["Garden Planning", "Garden notes"]);
            // Pagination keeps the total.
            let (page, total) = list_metadata_conn(conn, Some("garden"), 1, 1).unwrap();
            assert_eq!((page.len(), total), (1, 2));
        });
    }

    /// `list_version` moves when the listing changes (the History drawer
    /// re-queries on it), and a save that changes nothing listed (a
    /// streaming reply's other fields) leaves the row's listing hash alone.
    #[test]
    fn list_version_tracks_listing_changes_only() {
        with_test_db(|conn| {
            let mut s = session("Sep 24 - 09:00 AM", false, None, None);
            let v0 = list_version();
            upsert(conn, &s);
            let v1 = list_version();
            assert!(v1 > v0, "a new row bumps the version");

            let before = build_row(&s, next_seq()).unwrap().list_hash;
            s.scratchpad = "streaming scratch".into();
            s.project_id = Some("p".into());
            assert_eq!(build_row(&s, next_seq()).unwrap().list_hash, before);
            s.tab_title = Some("Retitled".into());
            assert_ne!(build_row(&s, next_seq()).unwrap().list_hash, before);
            upsert(conn, &s);
            let v2 = list_version();
            assert!(v2 > v1, "a retitle bumps the version");

            rename_session_conn(conn, &s.id, "Renamed").unwrap();
            let v3 = list_version();
            assert!(v3 > v2, "a rename from History bumps the version");
            assert_eq!(list_metadata_conn(conn, None, 0, 10).unwrap().0[0].name, "Renamed");

            delete_session_for_test(conn, &s.id);
            assert!(list_version() > v3, "a delete bumps the version");
            assert_eq!(list_metadata_conn(conn, None, 0, 10).unwrap().1, 0);
        });
    }

    #[test]
    fn search_matches_ai_topic_title() {
        with_test_db(|conn| {
            upsert(conn, &session("Sep 16 - 03:45 PM", false, Some("Quarterly Budget Review"), Some("💰")));
            upsert(conn, &session("Sep 17 - 09:00 AM", false, None, None));
            let (metas, total) = list_metadata_conn(conn, Some("budget"), 0, 10).unwrap();
            assert_eq!(total, 1);
            assert_eq!(metas[0].display_title(), "Quarterly Budget Review");
            assert_eq!(metas[0].display_emoji(), Some("💰"));
            let (_, none) = list_metadata_conn(conn, Some("nonexistent"), 0, 10).unwrap();
            assert_eq!(none, 0);
        });
    }

    #[test]
    fn backfill_adds_ai_topic_title_to_search_text_once() {
        with_test_db(|conn| {
            upsert(conn, &session("Sep 16 - 03:45 PM", false, Some("Quarterly Budget Review"), Some("💰")));
            upsert(conn, &session("Sep 17 - 09:00 AM", false, None, None));
            // Simulate rows written before build_row carried the title.
            conn.execute("UPDATE sessions SET search_text = lower(name)", []).unwrap();
            conn.execute("DELETE FROM meta WHERE key = ?1", params![META_SEARCH_TAB_TITLE]).unwrap();
            assert_eq!(list_metadata_conn(conn, Some("budget"), 0, 10).unwrap().1, 0);

            backfill_search_tab_titles(conn).unwrap();
            assert_eq!(list_metadata_conn(conn, Some("budget"), 0, 10).unwrap().1, 1);
            // Untitled rows are left alone, and a second run is a no-op.
            backfill_search_tab_titles(conn).unwrap();
            let titled: String = conn
                .query_row(
                    "SELECT search_text FROM sessions WHERE json_extract(data, '$.tab_title') IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(titled.matches("quarterly budget review").count(), 1);
        });
    }

    #[test]
    fn display_emoji_ignores_blank() {
        let mut m = SessionMeta {
            id: "a".into(),
            name: "n".into(),
            last_updated: String::new(),
            message_count: 0,
            summary: String::new(),
            tab_title: None,
            tab_emoji: None,
            name_user_set: false,
        };
        assert_eq!(m.display_emoji(), None);
        m.tab_emoji = Some("  ".into());
        assert_eq!(m.display_emoji(), None);
        m.tab_emoji = Some("🐯".into());
        assert_eq!(m.display_emoji(), Some("🐯"));
    }

    // ── Pin migration ───────────────────────────────────────────────────

    fn pin_of(conn: &Connection, id: &str) -> Option<String> {
        let data = get_row_data(conn, id).unwrap();
        serde_json::from_str::<Session>(&data).unwrap().composio_profile
    }

    fn row_seq(conn: &Connection, id: &str) -> i64 {
        conn.query_row("SELECT seq FROM sessions WHERE id = ?1", params![id], |r| r.get(0)).unwrap()
    }

    fn pin_settings(home: Option<&str>) -> crate::settings::Settings {
        let mut s = crate::settings::Settings::default();
        for (id, name) in [("cm", "Clearmirror"), ("pg", "Puget")] {
            s.composio_profiles.push(crate::settings::ComposioProfile {
                id: id.into(),
                name: name.into(),
                ..Default::default()
            });
        }
        s.home_composio_profile = home.map(str::to_string);
        // Focus-driven: must never be the target.
        s.active_composio_profile = Some("pg".into());
        s
    }

    /// A Composio call stamped with `profile`.
    fn composio_call(profile: &str) -> crate::components::chat::Message {
        let mut tc = crate::components::shared::ToolCall::new(
            format!("composio-native:{profile}"),
            "GMAIL_SEND_EMAIL".into(),
            serde_json::json!({}),
            None,
            None,
        );
        tc.profile_id = Some(profile.into());
        crate::components::chat::Message {
            id: uuid::Uuid::new_v4(),
            author: "Hobbes".into(),
            content: crate::components::shared::MessageContent::ToolCall(tc),
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: chrono::Utc::now(),
            usage: None,
        }
    }

    /// The whole backfill on one connection, the way the thread runs it
    /// (tiny batches, so the test crosses batch boundaries).
    fn run_pin_backfill(
        conn: &Connection,
        rules: &crate::settings::Settings,
        high_water: i64,
        skip: &std::collections::HashSet<String>,
    ) -> usize {
        let mut cursor = meta(conn, META_PIN_CURSOR).and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut pinned = 0;
        while let Some((last, rows)) = read_pin_batch(conn, cursor, 1).unwrap() {
            let fixes = pin_fixes(rules, rows, high_water, skip);
            pinned += write_pin_fixes(conn, &fixes, last).unwrap();
            cursor = last;
        }
        meta_set_conn(conn, META_PIN_BACKFILLED, "1", next_seq()).unwrap();
        pinned
    }

    #[test]
    fn pin_backfill_follows_each_tabs_own_profile() {
        with_test_db(|conn| {
            let rules = pin_settings(Some("cm"));
            // Its own most recent Composio call ran in Puget.
            let mut worked = session("Worked in Puget", false, Some("Topic"), None);
            worked.messages.push(composio_call("cm"));
            worked.messages.push(composio_call("pg"));
            worked.scratchpad = "notes".into();
            // No Composio calls: Hobbes' home.
            let quiet = session("Quiet", false, None, None);
            // A legacy name pin: normalised to the id.
            let mut named = session("Named", false, None, None);
            named.composio_profile = Some("Puget".into());
            let mut pinned = session("Pinned", false, None, None);
            pinned.composio_profile = Some("pg".into());
            let mut removed = session("Removed", false, None, None);
            removed.composio_profile = Some("gone".into());
            for s in [&worked, &quiet, &named, &pinned, &removed] {
                upsert(conn, s);
            }
            let before = row_seq(conn, &worked.id);
            let high_water = next_seq();

            assert_eq!(run_pin_backfill(conn, &rules, high_water, &Default::default()), 3);

            assert_eq!(pin_of(conn, &worked.id).as_deref(), Some("pg"), "its own calls, not Hobbes' home");
            assert_eq!(pin_of(conn, &quiet.id).as_deref(), Some("cm"), "Hobbes' home");
            assert_eq!(pin_of(conn, &named.id).as_deref(), Some("pg"));
            assert_eq!(pin_of(conn, &pinned.id).as_deref(), Some("pg"));
            assert_eq!(pin_of(conn, &removed.id).as_deref(), Some("gone"), "never repointed at another profile");
            assert!(row_seq(conn, &worked.id) > before, "a fresh seq, so an older in-flight save can't undo it");
            let back: Session = serde_json::from_str(&get_row_data(conn, &worked.id).unwrap()).unwrap();
            assert_eq!(back.scratchpad, "notes", "the rest of the session is untouched");
            assert_eq!(back.tab_title.as_deref(), Some("Topic"));
            // Journaled with the write.
            assert!(matches!(
                &load_events_for_test(conn, &worked.id, 0).last().unwrap().event,
                SessionEvent::ComposioProfileSet { profile: Some(p) } if p == "pg"
            ));
            assert_eq!(meta(conn, META_PIN_BACKFILLED).as_deref(), Some("1"));
        });
    }

    #[test]
    fn pin_backfill_without_a_rule_leaves_the_tab_unpinned() {
        with_test_db(|conn| {
            let quiet = session("Quiet", false, None, None);
            upsert(conn, &quiet);
            // No calls, no Hobbes home: today's behaviour (never the global).
            assert_eq!(run_pin_backfill(conn, &pin_settings(None), next_seq(), &Default::default()), 0);
            assert_eq!(pin_of(conn, &quiet.id), None);
        });
    }

    #[test]
    fn pin_backfill_never_touches_rows_with_an_in_memory_copy() {
        with_test_db(|conn| {
            let rules = pin_settings(Some("cm"));
            let hydrated = session("Hydrated", false, None, None);
            let system = session("Hobbes archive", false, None, None);
            let loaded = session("Loaded earlier", false, None, None);
            let later = session("Saved after the spawn", false, None, None);
            for s in [&hydrated, &system, &loaded] {
                upsert(conn, s);
            }
            let high_water = next_seq();
            upsert(conn, &later);
            // A system session the backfill wasn't told about: the registry
            // knows it (it may learn an archive after the spawn).
            crate::system_sessions::set_current(&system.id);
            mark_row_loaded(&loaded.id, &get_row_data(conn, &loaded.id).unwrap());
            let skip: std::collections::HashSet<String> = [hydrated.id.clone()].into();

            assert_eq!(run_pin_backfill(conn, &rules, high_water, &skip), 0);

            assert_eq!(pin_of(conn, &hydrated.id), None, "hydrated at spawn: pinned in memory instead");
            assert_eq!(pin_of(conn, &system.id), None, "system sessions keep their own rules");
            assert_eq!(pin_of(conn, &loaded.id), None, "a row with a fingerprint is left to its copy");
            assert_eq!(pin_of(conn, &later.id), None, "written since the spawn: an in-memory copy's");
            forget_fingerprint(&loaded.id);
        });
    }

    /// A save landing between the batch read and its write wins: the fix
    /// is dropped (its seq changed), not written over the newer row.
    #[test]
    fn pin_backfill_drops_a_fix_when_the_row_changed_since_its_read() {
        with_test_db(|conn| {
            let rules = pin_settings(Some("cm"));
            let mut s = session("Open tab", false, None, None);
            upsert(conn, &s);
            let (last, rows) = read_pin_batch(conn, 0, PIN_BACKFILL_BATCH_BYTES).unwrap().unwrap();
            let fixes = pin_fixes(&rules, rows, next_seq(), &Default::default());
            assert_eq!(fixes.len(), 1);
            // Meanwhile its copy saves, pinned elsewhere, with more content.
            s.composio_profile = Some("pg".into());
            s.scratchpad = "newer".into();
            upsert(conn, &s);
            assert_eq!(write_pin_fixes(conn, &fixes, last).unwrap(), 0);
            let back: Session = serde_json::from_str(&get_row_data(conn, &s.id).unwrap()).unwrap();
            assert_eq!(back.composio_profile.as_deref(), Some("pg"));
            assert_eq!(back.scratchpad, "newer");
            assert_eq!(meta(conn, META_PIN_CURSOR).as_deref(), Some(last.to_string().as_str()), "the cursor still moves");
        });
    }

    /// Batches are cut by bytes (at least one row), and a restart resumes
    /// from the stored cursor.
    #[test]
    fn pin_backfill_batches_by_bytes_and_resumes() {
        with_test_db(|conn| {
            let a = session("A", false, None, None);
            let b = session("B", false, None, None);
            upsert(conn, &a);
            upsert(conn, &b);
            let (first, rows) = read_pin_batch(conn, 0, 1).unwrap().unwrap();
            assert_eq!(rows.len(), 1, "one row even when it alone exceeds the budget");
            let (_, both) = read_pin_batch(conn, 0, i64::MAX).unwrap().unwrap();
            assert_eq!(both.len(), 2);
            // Interrupted after the first batch: the next run starts after it.
            write_pin_fixes(conn, &[], first).unwrap();
            let rules = pin_settings(Some("cm"));
            assert_eq!(run_pin_backfill(conn, &rules, next_seq(), &Default::default()), 1);
            assert_eq!(pin_of(conn, &rows[0].0), None, "behind the cursor: not revisited");
        });
    }

    #[test]
    fn a_tab_hydrating_unpinned_follows_the_pin_rule() {
        clear_pin_rules();
        let mut s = session("Old tab", false, None, None);
        assert!(!pin_on_load(&s.id.clone(), &mut s), "no rules yet: nothing to pin to");
        set_pin_rules(&pin_settings(None));
        assert!(!pin_on_load(&s.id.clone(), &mut s), "no calls, no Hobbes home: left unpinned");
        s.messages.push(composio_call("pg"));
        assert!(pin_on_load(&s.id.clone(), &mut s));
        assert_eq!(s.composio_profile.as_deref(), Some("pg"));
        // A name pin becomes its id; an id pin is untouched.
        s.composio_profile = Some("Clearmirror".into());
        assert!(pin_on_load(&s.id.clone(), &mut s));
        assert_eq!(s.composio_profile.as_deref(), Some("cm"));
        assert!(!pin_on_load(&s.id.clone(), &mut s));
        // The Hobbes session keeps its own rules.
        let mut hobbes = session("Hobbes", false, None, None);
        hobbes.messages.push(composio_call("pg"));
        crate::system_sessions::set_current(&hobbes.id);
        assert!(!pin_on_load(&hobbes.id.clone(), &mut hobbes));
        assert_eq!(hobbes.composio_profile, None);
        clear_pin_rules();
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
                signature_provider: None,
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
            reason: None,
            until: None,
            fired_at: None,
            fire_cause: None,
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
            SessionEvent::TabTitled {
                emoji: Some("🐯".to_string()),
                title: Some("Ship the release".to_string()),
            },
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

    /// A stream still writing to a deleted session must not be able to
    /// resurrect its journal: events appended after `delete_session` are
    /// silently dropped, not just ones collected before the delete.
    #[test]
    fn test_orphan_events_after_delete_are_dropped() {
        with_test_db(|conn| {
            let mut state = crate::session::SessionState::default();
            let sid = state.create_session_raw(None);
            upsert(conn, state.sessions.get(&sid).unwrap());

            append_events_conn(conn, &sid, &[SessionEvent::ScratchpadSet { content: "before".into() }]);
            assert_eq!(event_count(conn, &sid), 1);

            delete_session_for_test(conn, &sid);
            assert_eq!(event_count(conn, &sid), 0);

            // A stream that was mid-flight when the tab was deleted keeps
            // producing events for the now-tombstoned session id.
            append_events_conn(conn, &sid, &[SessionEvent::ScratchpadSet { content: "orphan".into() }]);
            assert_eq!(
                event_count(conn, &sid),
                0,
                "events appended after delete must not be written"
            );
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

    /// delete_session_for_test must forget fingerprints like the real delete,
    /// so a re-created session is not masked as clean by a stale fingerprint.
    #[test]
    fn test_delete_for_test_forgets_fingerprint() {
        with_test_db(|conn| {
            let mut state = crate::session::SessionState::default();
            let sid = state.create_session_raw(None);
            upsert(conn, state.sessions.get(&sid).unwrap());

            // Create a fingerprint by marking the row as saved.
            let row = build_row(state.sessions.get(&sid).unwrap(), next_seq()).unwrap();
            mark_rows_saved(std::slice::from_ref(&row));
            assert!(fingerprints().lock().unwrap().contains_key(&sid), "fingerprint must exist after mark_rows_saved");

            // Delete the session.
            delete_session_for_test(conn, &sid);

            // Fingerprint must be gone.
            assert!(
                !fingerprints().lock().unwrap().contains_key(&sid),
                "fingerprint must be forgotten after delete"
            );
        });
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    const V1: &str = "0.13.0";
    const V2: &str = "0.14.0";

    /// A store at `dir/sessions.db` with one session row, closed.
    fn existing_store(dir: &std::path::Path, version: &str) -> PathBuf {
        let path = dir.join("sessions.db");
        let conn = open_store(&path, version).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, name, last_updated, message_count, total_cost, total_tokens, has_timers, summary, search_text, seq, data)
             VALUES ('a', 'first', '2026-11-11T00:00:00Z', 0, 0, 0, 0, '', '', 1, '{}')",
            [],
        )
        .unwrap();
        path
    }

    fn backups(dir: &std::path::Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".pre-"))
            .collect();
        out.sort();
        out
    }

    fn ids_in(path: &std::path::Path) -> Vec<String> {
        let c = Connection::open(path).unwrap();
        let mut stmt = c.prepare("SELECT id FROM sessions ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn a_fresh_install_takes_none_and_its_next_launch_neither() {
        let dir = tempfile::tempdir().unwrap();
        let path = existing_store(dir.path(), V1);
        assert!(backups(dir.path()).is_empty());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(backup_before_upgrade(&conn, &path, V1), BackupOutcome::UpToDate);
        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn taken_on_a_version_change_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = existing_store(dir.path(), V1);
        let conn = open_store(&path, V2).unwrap();
        assert_eq!(backups(dir.path()), ["sessions.db.pre-0.14.0.bak"]);
        assert_eq!(meta_get_conn(&conn, META_BACKUP_VERSION).unwrap().as_deref(), Some(V2));
        assert_eq!(ids_in(&dir.path().join("sessions.db.pre-0.14.0.bak")), ["a"]);
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(backup_before_upgrade(&conn, &path, V2), BackupOutcome::UpToDate);
    }

    /// Rows still in the WAL (another connection holds it open, so nothing
    /// was checkpointed on close) are in the backup.
    #[test]
    fn the_backup_includes_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = existing_store(dir.path(), V1);
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
        writer
            .execute(
                "INSERT INTO sessions (id, name, last_updated, message_count, total_cost, total_tokens, has_timers, summary, search_text, seq, data)
                 VALUES ('b', 'in the wal', '2026-11-11T00:00:00Z', 0, 0, 0, 0, '', '', 2, '{}')",
                [],
            )
            .unwrap();
        let wal = dir.path().join("sessions.db-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0, "the row is in the WAL");
        let conn = Connection::open(&path).unwrap();
        let BackupOutcome::Taken { path: bak, bytes } = backup_before_upgrade(&conn, &path, V2) else { panic!("no backup") };
        assert!(bytes > 0);
        assert_eq!(ids_in(&bak), ["a", "b"]);
        drop(writer);
    }

    #[test]
    fn only_the_newest_backup_is_kept_and_a_temp_file_is_never_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = existing_store(dir.path(), V1);
        drop(open_store(&path, V2).unwrap());
        // A crash mid-copy left a partial temp file of a later version.
        std::fs::write(dir.path().join("sessions.db.pre-0.15.0.bak.tmp"), b"partial").unwrap();
        let (baks, tmps) = backup_files(&path);
        assert_eq!(baks.len(), 1, "the temp file isn't a backup");
        assert_eq!(tmps.len(), 1);
        drop(open_store(&path, "0.15.0").unwrap());
        assert_eq!(backups(dir.path()), ["sessions.db.pre-0.15.0.bak"], "older pruned, temp gone");
        assert_eq!(ids_in(&dir.path().join("sessions.db.pre-0.15.0.bak")), ["a"]);
    }

    /// A backup that can't be written doesn't stop the open, isn't recorded
    /// (so the next launch retries), and the retry, which postdates this
    /// version's migrations, keeps the older backup.
    #[test]
    fn a_failed_backup_never_stops_the_open_and_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = existing_store(dir.path(), V1);
        drop(open_store(&path, V2).unwrap());
        // The temp path is taken by a directory: the copy fails.
        let blocker = dir.path().join("sessions.db.pre-0.15.0.bak.tmp");
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("x"), b"x").unwrap();
        let conn = open_store(&path, "0.15.0").expect("the open goes on");
        assert_eq!(meta_get_conn(&conn, META_BACKUP_VERSION).unwrap().as_deref(), Some(V2), "not recorded");
        assert_eq!(meta_get_conn(&conn, META_BACKUP_MISSED).unwrap().as_deref(), Some("0.15.0"));
        assert_eq!(ids_in(&path), ["a"], "the store works");
        assert_eq!(backups(dir.path()), ["sessions.db.pre-0.14.0.bak", "sessions.db.pre-0.15.0.bak.tmp"]);
        drop(conn);

        std::fs::remove_dir_all(&blocker).unwrap();
        let conn = open_store(&path, "0.15.0").unwrap();
        assert_eq!(meta_get_conn(&conn, META_BACKUP_VERSION).unwrap().as_deref(), Some("0.15.0"));
        assert_eq!(meta_get_conn(&conn, META_BACKUP_MISSED).unwrap(), None);
        assert_eq!(
            backups(dir.path()),
            ["sessions.db.pre-0.14.0.bak", "sessions.db.pre-0.15.0.bak"],
            "the older, pre-migration backup is kept"
        );
    }
}
