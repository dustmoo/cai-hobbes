//! The **Hobbes** session: the one permanent system tab. Typed messages,
//! voice conversations (transcripts and brain hand-offs, `voice`) and
//! Heartbeat wakes (`heartbeat::runtime`) all land in its single timeline;
//! Voice and the Heartbeat are capabilities of it, each turned on by its own
//! setting.
//!
//! - **One pointer, one creator.** The id lives in one `meta` key
//!   ([`META_KEY`]) and in [`HOBBES_SESSION_ID`]. Both the Heartbeat tick and
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

/// The Hobbes session's id once known (from the store at first use, past
/// any rotation the pointer missed).
pub static HOBBES_SESSION_ID: GlobalSignal<Option<String>> =
    Signal::global(|| stored_pointer().map(|id| crate::system_sessions::resolve_successor(&id)));

/// The Hobbes session's id, if it exists (peeked: no subscription).
pub fn current_id() -> Option<String> {
    HOBBES_SESSION_ID.peek().clone()
}

/// Whether `id` is the live Hobbes session.
pub fn is_hobbes(id: &str) -> bool {
    HOBBES_SESSION_ID.peek().as_deref() == Some(id)
}

/// The pre-merge Heartbeat and Voice session ids (resolved past their own
/// rotations), with the title their archives are named after. Read from
/// the raw persisted values: the Heartbeat state's old `session_id` field
/// and the Voice pointer.
pub fn legacy_sessions() -> Vec<(String, &'static str)> {
    let archives = crate::system_sessions::stored_archives();
    let heartbeat = crate::session_store::meta_get(crate::heartbeat::META_KEY)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(str::to_string));
    let voice = crate::session_store::meta_get(LEGACY_VOICE_META_KEY);
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
    crate::system_sessions::register(id);
    if crate::system_sessions::repair(session_state, id, SESSION_EMOJI, home) {
        SessionState::save_signal(session_state, None);
    }
}

/// The Hobbes session's id, hydrating it into memory if it is only on disk,
/// merging the pre-merge sessions on first use ([`migrate`]), and creating
/// it if it never existed or was deleted. Never touches the active session
/// or the open tabs (except the merge's in-place replacement of an open
/// Heartbeat / Voice tab). `None` when the store is unwritable.
///
/// The only creator: the Heartbeat tick and the voice controller both call
/// it, synchronously on the Dioxus thread, so there is no create race.
pub fn ensure_session(mut session_state: Signal<SessionState>, settings: &Settings) -> Option<String> {
    if session_state.peek().save_disabled {
        // Startup couldn't read the store: don't create rows we can't save.
        return None;
    }
    let home = settings.home_profile_id();
    if let Some(id) = current_id() {
        // One indexed row: fine on the UI thread (P-017).
        if SessionState::ensure_hydrated(session_state, &id) {
            adopt(&mut session_state, &id, home);
            return Some(id);
        }
        tracing::info!("hobbes tab: session {id} is gone; creating a new one");
    } else if let Some(id) = migrate(session_state, settings, chrono::Utc::now()) {
        adopt(&mut session_state, &id, home);
        return Some(id);
    }
    create(session_state, settings)
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
    *HOBBES_SESSION_ID.write() = Some(id.clone());
    tracing::info!("hobbes tab: created session {id}");
    Some(id)
}

/// The one-time merge: the pre-merge Heartbeat and Voice sessions become
/// read-only archives of the Hobbes lineage and a fresh Hobbes session
/// takes over, carrying their recent cross-company reads, dispatch-relay
/// workers and pending timers (`session_rotation::prepare_merge`). Runs only
/// while there is no Hobbes pointer; the pointer is written in the same
/// transaction as the archives, so it never runs twice. `None` when there
/// is nothing to merge (or the write failed: everything stays as it was).
pub fn migrate(session_state: Signal<SessionState>, settings: &Settings, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    if current_id().is_some() || session_state.peek().save_disabled {
        return None;
    }
    let olds: Vec<(&'static str, crate::session::Session)> = legacy_sessions()
        .into_iter()
        .filter_map(|(id, title)| {
            let in_memory = session_state.peek().sessions.get(&id).cloned();
            in_memory.or_else(|| crate::session_store::load_session(&id)).map(|s| (title, s))
        })
        .collect();
    if olds.is_empty() {
        return None;
    }
    let refs: Vec<(&str, &crate::session::Session)> = olds.iter().map(|(t, s)| (*t, s)).collect();
    let prepared = crate::session_rotation::prepare_merge(
        &refs,
        settings,
        &uuid::Uuid::new_v4().to_string(),
        new_session_profile(settings),
        now,
    );
    crate::session_rotation::apply_merge(session_state, prepared)
}

/// Startup: merge the pre-merge sessions if this is the first run of the
/// Hobbes tab, else repair the Hobbes session if its tab is open
/// (hydrated). Doesn't create one: that waits for the Heartbeat or voice
/// to need it.
pub fn startup(mut session_state: Signal<SessionState>, settings: &Settings) {
    if session_state.peek().save_disabled {
        return;
    }
    match current_id() {
        Some(id) => {
            if session_state.peek().sessions.contains_key(&id) {
                adopt(&mut session_state, &id, settings.home_profile_id());
            }
        }
        None => {
            if let Some(id) = migrate(session_state, settings, chrono::Utc::now()) {
                adopt(&mut session_state, &id, settings.home_profile_id());
            }
        }
    }
}
