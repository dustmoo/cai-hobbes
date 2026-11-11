//! The **Hobbes** session: the one permanent system tab. Typed messages,
//! voice conversations (transcripts and brain hand-offs, `voice`) and
//! Heartbeat wakes (`heartbeat::runtime`) all land in its single timeline;
//! Voice and the Heartbeat are capabilities of it, each turned on by its own
//! setting.
//!
//! - **One pointer, one creator.** The id lives in one `meta` key
//!   ([`META_KEY`]) and in `system_sessions`' registry (the single source of truth; written only by `system_sessions::set_current` / `apply_rotation`). Both the Heartbeat tick and
//!   the voice controller get it from [`ensure_session`]; it is synchronous
//!   and runs on the Dioxus thread, so the two can't both create one.
//! - **Trust follows the turn.** Being the Hobbes session decides nothing
//!   about strictness: each turn carries its origin (`session::TurnOrigin`),
//!   and only an unattended one (a Heartbeat wake) gets the Heartbeat's scope
//!   rules (`builtin_tools::heartbeat_gate`).
//! - **The merge.** Builds before this one kept separate Heartbeat and Voice
//!   sessions. The first run with no Hobbes pointer turns both into
//!   read-only archives (the rotation mechanism, `session_rotation`) and
//!   starts the Hobbes session fresh, carrying their recent cross-company
//!   reads so the hold doesn't reset ([`migrate`]). Idempotent: the pointer
//!   is written in the same transaction as the archives.

use dioxus::prelude::*;

use crate::session::SessionState;
use crate::settings::Settings;

/// Title of the session (never auto-titled: it is a user-set name).
pub const SESSION_TITLE: &str = "Hobbes";
/// Its tab emoji, journaled so a rebuild keeps it. The tab components draw
/// the app icon in its place ([`crate::components::hobbes_icon`]).
pub const SESSION_EMOJI: &str = "🐯";
/// The pre-merge Heartbeat session's emoji: its archives keep the metal
/// heart.
pub const LEGACY_HEARTBEAT_EMOJI: &str = "🫀";
/// The pre-merge Voice session's emoji: its archives keep the metal mouth.
pub const LEGACY_VOICE_EMOJI: &str = "👄";
/// `session_store` meta key holding the session id across launches.
pub const META_KEY: &str = "hobbes_session_id";
/// Where the pre-merge Voice session's id was kept.
const LEGACY_VOICE_META_KEY: &str = "voice_session_id";

/// The persisted pointer, if any (not resolved through rotations).
pub fn stored_pointer() -> Option<String> {
    crate::session_store::meta_get(META_KEY).filter(|s| !s.is_empty())
}

/// The pre-merge Heartbeat and Voice session ids (resolved past their own
/// rotations), with the title their archives are named after. Read from
/// the raw persisted values: the Heartbeat state's old `session_id` field
/// and the Voice pointer.
pub fn legacy_sessions() -> Vec<(String, &'static str)> {
    legacy_sessions_with(crate::session_store::meta_get)
}

/// [`legacy_sessions`] over any `meta` reader (tests pass a test database's).
pub(crate) fn legacy_sessions_with(meta: impl Fn(&str) -> Option<String>) -> Vec<(String, &'static str)> {
    let archives = meta(crate::system_sessions::ARCHIVES_META_KEY)
        .and_then(|json| crate::system_sessions::Archives::parse(&json))
        .unwrap_or_default();
    let heartbeat = meta(crate::heartbeat::META_KEY)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(str::to_string));
    let voice = meta(LEGACY_VOICE_META_KEY);
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for (id, title) in [(heartbeat, "Heartbeat"), (voice, "Voice")] {
        let Some(id) = id.filter(|s| !s.is_empty()).map(|id| archives.resolve(&id)) else { continue };
        if !out.iter().any(|(o, _)| *o == id) {
            out.push((id, title));
        }
    }
    out
}

/// [`legacy_sessions`]' ids.
pub fn legacy_ids() -> Vec<String> {
    legacy_sessions().into_iter().map(|(id, _)| id).collect()
}

/// The Composio profile a new Hobbes session starts in: Hobbes' home
/// profile, else the active one (like every other new tab).
fn new_session_profile(settings: &Settings) -> Option<String> {
    settings
        .home_profile_id()
        .map(str::to_string)
        .or_else(|| settings.get_active_profile().map(|p| p.id.clone()))
}

/// Mark `id` as the Hobbes system session and repair its icon (journaled),
/// project tag and home profile. Cheap when nothing needs fixing.
fn adopt(session_state: &mut Signal<SessionState>, id: &str, home: Option<&str>) {
    crate::system_sessions::set_current(id);
    if crate::system_sessions::repair(session_state, id, SESSION_EMOJI, home) {
        SessionState::save_signal(session_state, None);
    }
}

/// The Hobbes session's id, hydrating it into memory if it is only on disk,
/// merging the pre-merge sessions on first use ([`migrate`]), and creating
/// it if it never existed or was deleted. Never touches the active session
/// or the open tabs (except the merge's in-place replacement of an open
/// Heartbeat / Voice tab). `None` when the store is unwritable, or while
/// a failed merge waits to be retried ([`MERGE_RETRY_AFTER`]).
///
/// The only creator: the Heartbeat tick and the voice controller both call
/// it, synchronously on the Dioxus thread, so there is no create race.
pub fn ensure_session(mut session_state: Signal<SessionState>, settings: &Settings) -> Option<String> {
    if session_state.peek().save_disabled {
        // Startup couldn't read the store: don't create rows we can't save.
        return None;
    }
    let home = settings.home_profile_id();
    if let Some(id) = crate::system_sessions::current() {
        // One indexed row: fine on the UI thread (P-017).
        if SessionState::ensure_hydrated(session_state, &id) {
            adopt(&mut session_state, &id, home);
            return Some(id);
        }
        tracing::info!("hobbes tab: session {id} is gone; creating a new one");
    } else {
        if merge_retry_waiting(std::time::Instant::now()) {
            return None;
        }
        match migrate(session_state, settings, chrono::Utc::now()) {
            Merge::Merged(id) => {
                adopt(&mut session_state, &id, home);
                return Some(id);
            }
            // The pre-merge sessions are still there, unmerged: creating a
            // session now would write the pointer and strand them as
            // ordinary tabs (timers pending, workers routed to them) with
            // no later merge. Wait and retry instead.
            Merge::Failed => {
                note_merge_failure(std::time::Instant::now());
                return None;
            }
            Merge::Nothing => {}
        }
    }
    create(session_state, settings)
}

/// How long [`ensure_session`] waits after a failed merge before trying
/// again (the Heartbeat tick calls it every 30 s). A restart retries at
/// once ([`startup`]).
const MERGE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5 * 60);

fn merge_failed_at() -> &'static std::sync::Mutex<Option<std::time::Instant>> {
    static AT: std::sync::OnceLock<std::sync::Mutex<Option<std::time::Instant>>> = std::sync::OnceLock::new();
    AT.get_or_init(|| std::sync::Mutex::new(None))
}

fn note_merge_failure(now: std::time::Instant) {
    if let Ok(mut at) = merge_failed_at().lock() {
        *at = Some(now);
    }
}

/// A merge failed less than [`MERGE_RETRY_AFTER`] ago.
fn merge_retry_waiting(now: std::time::Instant) -> bool {
    merge_failed_at()
        .lock()
        .ok()
        .and_then(|at| *at)
        .is_some_and(|at| now.duration_since(at) < MERGE_RETRY_AFTER)
}

/// Create a fresh Hobbes session and point everything at it.
fn create(mut session_state: Signal<SessionState>, settings: &Settings) -> Option<String> {
    let id = {
        let mut state = session_state.write();
        // create_session_raw makes the new session active; put it back.
        let previous_active = state.active_session_id.clone();
        let id = state.create_session_raw(new_session_profile(settings));
        if !previous_active.is_empty() {
            state.active_session_id = previous_active;
        }
        // A user-set name, so auto-titling never renames it (nor its icon).
        state.update_session_name_raw(&id, SESSION_TITLE.to_string());
        id
    };
    adopt(&mut session_state, &id, settings.home_profile_id());
    SessionState::save_signal(&session_state, None);
    if let Err(e) = crate::session_store::meta_set(META_KEY, &id) {
        tracing::warn!("hobbes tab: could not persist the session id: {e}");
    }
    crate::system_sessions::set_current(&id);
    tracing::info!("hobbes tab: created session {id}");
    Some(id)
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Merge {
    /// The pre-merge sessions are archives now; the Hobbes session's id.
    Merged(String),
    /// Nothing to merge: no pre-merge session (or none left to load), or a
    /// Hobbes session already exists.
    Nothing,
    /// The write failed. Nothing changed, on disk or in memory, so a retry
    /// starts from the same state: it can't duplicate or lose messages.
    Failed,
}

/// The one-time merge: the pre-merge Heartbeat and Voice sessions become
/// read-only archives of the Hobbes lineage and a fresh Hobbes session
/// takes over, carrying their recent cross-company reads, dispatch-relay
/// workers and pending timers (`session_rotation::prepare_merge`). Runs only
/// while there is no Hobbes pointer; the pointer is written in the same
/// transaction as the archives, so it never runs twice. A failed write
/// leaves everything as it was, and no Hobbes session is created in its
/// place ([`ensure_session`]), so the next attempt (later, or the next
/// launch) merges again from scratch.
pub fn migrate(session_state: Signal<SessionState>, settings: &Settings, now: chrono::DateTime<chrono::Utc>) -> Merge {
    if crate::system_sessions::current().is_some() || session_state.peek().save_disabled {
        return Merge::Nothing;
    }
    let olds: Vec<(&'static str, crate::session::Session)> = legacy_sessions()
        .into_iter()
        .filter_map(|(id, title)| {
            let in_memory = session_state.peek().sessions.get(&id).cloned();
            in_memory.or_else(|| crate::session_store::load_session(&id)).map(|s| (title, s))
        })
        .collect();
    if olds.is_empty() {
        return Merge::Nothing;
    }
    let refs: Vec<(&str, &crate::session::Session)> = olds.iter().map(|(t, s)| (*t, s)).collect();
    let prepared = crate::session_rotation::prepare_merge(
        &refs,
        settings,
        &uuid::Uuid::new_v4().to_string(),
        new_session_profile(settings),
        now,
    );
    match crate::session_rotation::apply_merge(session_state, prepared) {
        Some(id) => Merge::Merged(id),
        None => Merge::Failed,
    }
}

/// Startup: merge the pre-merge sessions if this is the first run of the
/// Hobbes tab, else repair the Hobbes session if its tab is open
/// (hydrated). Doesn't create one: that waits for the Heartbeat or voice
/// to need it.
pub fn startup(mut session_state: Signal<SessionState>, settings: &Settings) {
    if session_state.peek().save_disabled {
        return;
    }
    match crate::system_sessions::current() {
        Some(id) => {
            if session_state.peek().sessions.contains_key(&id) {
                adopt(&mut session_state, &id, settings.home_profile_id());
            }
        }
        None => match migrate(session_state, settings, chrono::Utc::now()) {
            Merge::Merged(id) => adopt(&mut session_state, &id, settings.home_profile_id()),
            Merge::Failed => note_merge_failure(std::time::Instant::now()),
            Merge::Nothing => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::chat::Message;
    use crate::components::shared::MessageContent;
    use crate::session_store::test_support as store;
    use crate::timers::TimerStatus;

    fn message(author: &str, text: &str, at: chrono::DateTime<chrono::Utc>) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            author: author.into(),
            content: MessageContent::Text {
                content: text.into(),
                thought_signature: None,
                thought_summary: None,
                signature_provider: None,
            },
            attachments: Vec::new(),
            comments: Vec::new(),
            created_at: at,
            usage: None,
        }
    }

    /// The merge's whole data path against a `main`-shaped store: a
    /// Heartbeat session pointed at by `heartbeat_state.session_id`, no
    /// Hobbes pointer, no archive list. A first write that fails changes
    /// nothing; the retry merges once, with every message where it was.
    #[test]
    fn a_main_store_merges_once_and_a_failed_write_is_retried_cleanly() {
        let now = chrono::Utc::now();
        let mut state = SessionState::new();
        let hb = state.create_session_raw(None);
        let other = state.create_session_raw(None);
        {
            let s = state.sessions.get_mut(&hb).unwrap();
            s.name = "Heartbeat".into();
            s.name_user_set = true;
            s.messages = (0..5)
                .map(|i| message(if i % 2 == 0 { "User" } else { "Hobbes" }, &format!("wake {i}"), now - chrono::Duration::hours(5 - i)))
                .collect();
            s.scheduled_timers = vec![serde_json::from_value(serde_json::json!({
                "id": "t1", "session_id": hb, "created_at": now, "fire_at": now + chrono::Duration::hours(1),
                "mode": "prompt", "status": "pending",
            }))
            .unwrap()];
            let o = state.sessions.get_mut(&other).unwrap();
            o.messages = vec![message("User", "an ordinary chat", now)];
        }
        store::with_test_db(|conn| {
            store::upsert(conn, &state.sessions[&hb]);
            store::upsert(conn, &state.sessions[&other]);
            // `main`'s Heartbeat state: the session id inside its JSON.
            store::meta_set(conn, crate::heartbeat::META_KEY, &serde_json::json!({ "session_id": hb, "trail": [] }).to_string());
            let meta = |k: &str| store::meta(conn, k);
            assert_eq!(meta(META_KEY), None, "no Hobbes pointer yet");

            let legacy = legacy_sessions_with(meta);
            assert_eq!(legacy, vec![(hb.clone(), "Heartbeat")]);
            let load = || crate::session_store::peek_session_conn(conn, &hb).unwrap().unwrap();
            let settings = Settings::default();
            let commit = |new_id: &str| {
                let old = load();
                let p = crate::session_rotation::prepare_merge(&[("Heartbeat", &old)], &settings, new_id, None, now);
                let mut ids = crate::system_sessions::Ids::default();
                ids.rotate(&[hb.as_str()], new_id);
                let mut rows: Vec<&crate::session::Session> = p.archives.iter().map(|(_, a, _)| a).collect();
                rows.push(&p.successor);
                let (rows, meta) = crate::session_rotation::rows_and_meta(&rows, &ids.archives, new_id).unwrap();
                crate::session_store::write_rows_and_meta_conn(conn, &rows, &meta)
            };

            // A first attempt whose write fails part-way (after the archive
            // row, at the new session's row): nothing of it lands.
            conn.execute_batch(
                "CREATE TEMP TRIGGER fail_merge BEFORE INSERT ON sessions WHEN NEW.id = 'hobbes-1'
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
            assert!(commit("hobbes-1").is_err());
            let untouched = load();
            assert_eq!(untouched.name, "Heartbeat");
            assert_eq!(untouched.messages.len(), 5);
            assert_eq!(untouched.scheduled_timers[0].status, TimerStatus::Pending);
            assert_eq!(store::count(conn), 2);
            assert_eq!((meta(META_KEY), meta(crate::system_sessions::ARCHIVES_META_KEY)), (None, None));
            assert_eq!(legacy_sessions_with(meta), legacy, "still waiting to be merged");

            // The retry (a fresh id, as `migrate` makes) merges once.
            conn.execute_batch("DROP TRIGGER fail_merge;").unwrap();
            commit("hobbes-2").unwrap();
            assert_eq!(store::count(conn), 3, "the archive, the ordinary chat and one Hobbes session");
            assert!(store::get_row_data(conn, "hobbes-1").is_none());
            let archive = load();
            assert!(archive.name.starts_with("Heartbeat · "), "{}", archive.name);
            let texts = |s: &crate::session::Session| -> Vec<String> {
                s.messages.iter().filter_map(|m| m.content.get_text_content()).collect()
            };
            assert_eq!(texts(&archive), texts(&state.sessions[&hb]), "every message stays, once, in order");
            assert_eq!(archive.scheduled_timers[0].status, TimerStatus::Cancelled);
            let hobbes = crate::session_store::peek_session_conn(conn, "hobbes-2").unwrap().unwrap();
            assert_eq!(hobbes.name, SESSION_TITLE);
            assert_eq!(hobbes.messages.len(), 1, "just the intro");
            assert_eq!(hobbes.scheduled_timers.len(), 1);
            assert_eq!(hobbes.scheduled_timers[0].session_id, "hobbes-2");
            assert_eq!(
                crate::session_store::peek_session_conn(conn, &other).unwrap().unwrap().messages.len(),
                1,
                "other chats are untouched"
            );
            // The pointer and the archive list landed with the rows, so the
            // merge never runs again: the registry seeds `current` from the
            // pointer, and `migrate` only runs without one.
            assert_eq!(meta(META_KEY).as_deref(), Some("hobbes-2"));
            let archives =
                crate::system_sessions::Archives::parse(&meta(crate::system_sessions::ARCHIVES_META_KEY).unwrap()).unwrap();
            assert_eq!(archives.hobbes, vec![hb.clone()]);
            assert_eq!(archives.resolve(&hb), "hobbes-2");
        });
    }

    #[test]
    fn a_failed_merge_waits_before_the_next_try() {
        let t0 = std::time::Instant::now();
        note_merge_failure(t0);
        assert!(merge_retry_waiting(t0 + std::time::Duration::from_secs(30)));
        assert!(!merge_retry_waiting(t0 + MERGE_RETRY_AFTER));
        if let Ok(mut at) = merge_failed_at().lock() {
            *at = None;
        }
    }
}
