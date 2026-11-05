//! System sessions: the chat session Hobbes runs for itself — the **Hobbes**
//! tab, where Heartbeat wakes, voice conversations and typed messages share
//! one timeline (`hobbes_session`). It is a fixed part of the app, not a
//! topic: it keeps its name and icon (drawn from the session id, so a lost
//! or changed emoji can't hide it), is never filed under a project, and is
//! never retitled from a summary.
//!
//! A system session that grows too large is **rotated**
//! (`session_rotation`): a fresh session takes over and the old one stays in
//! History as a read-only archive. The registry keeps the lineage — the
//! current id plus every archived one — so archives keep their icon and
//! their exemptions, and anything still addressed to an archived id (a
//! dispatch result, a relay, a queued message) is routed to the current
//! session through [`current_for`]. The Heartbeat and Voice sessions of
//! earlier builds are archives of this lineage too (`hobbes_session`'s
//! one-time migration), so they route to the Hobbes session.
//!
//! A plain registry (no Signals) so background code and tests can ask too.
//! Seeded on first use from the persisted pointer and the archive list (meta
//! key [`ARCHIVES_META_KEY`]), and updated whenever the session is created,
//! migrated or rotated.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// `meta` key holding the archived ids (JSON [`Archives`]).
pub const ARCHIVES_META_KEY: &str = "system_session_archives";

/// Archived (rotated-out) ids, oldest first. Persisted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Archives {
    #[serde(default)]
    pub hobbes: Vec<String>,
    /// Archives of the separate Heartbeat / Voice lineages written by
    /// builds before the merge: read, folded into `hobbes` by
    /// [`Archives::normalize`], never written again.
    #[serde(default, skip_serializing)]
    pub heartbeat: Vec<String>,
    #[serde(default, skip_serializing)]
    pub voice: Vec<String>,
    /// Archived id → the session that replaced it. Written in the same
    /// transaction as the rows and the pointer, so a pointer that a late,
    /// older write set back to an archive is followed forward again at the
    /// next load ([`resolve_successor`]).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub successors: std::collections::BTreeMap<String, String>,
}

impl Archives {
    /// Follow `id` through the recorded rotations to the newest session.
    pub fn resolve(&self, id: &str) -> String {
        let mut cur = id.to_string();
        // Bounded: a corrupt map with a cycle can't spin.
        for _ in 0..=self.successors.len() {
            match self.successors.get(&cur) {
                Some(next) if next != &cur => cur = next.clone(),
                _ => break,
            }
        }
        cur
    }

    /// Fold the pre-merge Heartbeat / Voice archive lists into `hobbes`
    /// (Heartbeat's first), keeping order and dropping duplicates.
    pub fn normalize(mut self) -> Self {
        let legacy: Vec<String> = self.heartbeat.drain(..).chain(self.voice.drain(..)).collect();
        if !legacy.is_empty() {
            let mut merged = legacy;
            merged.append(&mut self.hobbes);
            let mut seen = std::collections::HashSet::new();
            merged.retain(|id| seen.insert(id.clone()));
            self.hobbes = merged;
        }
        self
    }

    fn parse(json: &str) -> Option<Self> {
        serde_json::from_str::<Archives>(json).ok().map(Archives::normalize)
    }
}

/// The registry's contents: the current id and the archives. Pure, so the
/// lineage rules are unit-tested on a local value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ids {
    pub current: Option<String>,
    pub archives: Archives,
    /// The persisted ids have been read (the store was open).
    seeded: bool,
}

impl Ids {
    fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// `id` is the Hobbes session, current or archived.
    pub fn is_system(&self, id: &str) -> bool {
        !id.is_empty() && (self.current() == Some(id) || self.archives.hobbes.iter().any(|a| a == id))
    }

    /// `id` is a rotated-out archive (and not, by some race, also current).
    pub fn is_archive(&self, id: &str) -> bool {
        self.is_system(id) && self.current() != Some(id)
    }

    /// `id` is the live session.
    pub fn is_current(&self, id: &str) -> bool {
        !id.is_empty() && self.current() == Some(id)
    }

    /// Where something addressed to `id` should land: the current session
    /// when `id` is an archive, else `id` itself.
    pub fn current_for(&self, id: &str) -> String {
        if self.is_archive(id) {
            self.current().unwrap_or(id).to_string()
        } else {
            id.to_string()
        }
    }

    /// Every id the lineage has had: archives (oldest first), then the
    /// current one.
    pub fn lineage(&self) -> Vec<String> {
        let mut out: Vec<String> =
            self.archives.hobbes.iter().filter(|a| self.current() != Some(a.as_str())).cloned().collect();
        if let Some(c) = self.current() {
            out.push(c.to_string());
        }
        out
    }

    /// Rotate: each of `olds` joins the archives (in order), `new` becomes
    /// current. A merge (the pre-merge Heartbeat and Voice sessions into the
    /// Hobbes one) is a rotation with two olds.
    pub fn rotate(&mut self, olds: &[&str], new: &str) {
        for old in olds {
            if !old.is_empty() && *old != new {
                if !self.archives.hobbes.iter().any(|a| a == old) {
                    self.archives.hobbes.push(old.to_string());
                }
                self.archives.successors.insert(old.to_string(), new.to_string());
            }
        }
        self.current = Some(new.to_string());
    }
}

/// The process-wide registry. Tests get one per thread, so parallel tests
/// registering their own "current" session don't race.
#[cfg(not(test))]
fn registry() -> &'static RwLock<Ids> {
    static REG: std::sync::OnceLock<RwLock<Ids>> = std::sync::OnceLock::new();
    REG.get_or_init(|| RwLock::new(Ids::default()))
}

/// Startup, once the store is open and before any session hydrates: the
/// one-time clear of the lineage's legacy summary sources
/// (`session_store::clear_system_summary_sources_once`).
pub fn clear_legacy_summary_sources() {
    if !crate::session_store::is_ready() {
        return;
    }
    crate::session_store::clear_system_summary_sources_once(&lineage());
}

#[cfg(test)]
fn registry() -> &'static RwLock<Ids> {
    thread_local! {
        static REG: &'static RwLock<Ids> = Box::leak(Box::new(RwLock::new(Ids::default())));
    }
    REG.with(|r| *r)
}

/// The persisted archive list (normalized), empty when there is none.
pub fn stored_archives() -> Archives {
    crate::session_store::meta_get(ARCHIVES_META_KEY).and_then(|json| Archives::parse(&json)).unwrap_or_default()
}

/// Fill unknown ids from the store, once it is open.
fn seed() {
    if registry().read().map(|ids| ids.seeded).unwrap_or(true) || !crate::session_store::is_ready() {
        return;
    }
    let archives = stored_archives();
    let current = crate::hobbes_session::stored_pointer().map(|id| archives.resolve(&id));
    // Before the one-time merge has run, the pre-merge Heartbeat / Voice
    // sessions are already archives: read-only, never live system sessions
    // of this build (`hobbes_session::migrate` gives them a successor).
    let legacy = if current.is_none() { crate::hobbes_session::legacy_ids() } else { Vec::new() };
    if let Ok(mut ids) = registry().write() {
        if ids.current.is_none() {
            ids.current = current;
        }
        for (old, new) in &archives.successors {
            ids.archives.successors.entry(old.clone()).or_insert_with(|| new.clone());
        }
        for a in archives.hobbes.iter().chain(legacy.iter()) {
            if !ids.archives.hobbes.contains(a) && ids.current.as_deref() != Some(a.as_str()) {
                ids.archives.hobbes.push(a.clone());
            }
        }
        ids.seeded = true;
    }
}

fn read<T>(f: impl FnOnce(&Ids) -> T) -> Option<T> {
    seed();
    registry().read().ok().map(|ids| f(&ids))
}

/// The reactive mirror of the current id for the UI. Private: only
/// [`set_current`] and [`apply_rotation`] write it, through [`notify`];
/// read it with [`current_reactive`].
static MIRROR: dioxus::prelude::GlobalSignal<Option<String>> = dioxus::prelude::Signal::global(current);

/// Push the registry's current id into the mirror. A no-op without a Dioxus
/// runtime (plain unit tests, background threads): the registry is still
/// correct, and the mirror initialises from it when first read.
fn notify() {
    use dioxus::prelude::Readable;
    if dioxus::dioxus_core::Runtime::current().is_err() {
        return;
    }
    let now = current();
    if *MIRROR.peek() != now {
        *MIRROR.write() = now;
    }
}

/// The live Hobbes session's id, if there is one (not reactive).
pub fn current() -> Option<String> {
    read(|ids| ids.current.clone()).flatten()
}

/// [`current`] as a subscribing read, for the UI: a component re-renders
/// when the id is assigned or rotated.
pub fn current_reactive() -> Option<String> {
    use dioxus::prelude::Readable;
    MIRROR.read().clone()
}

/// The one writer of the current Hobbes session id: the registry and the
/// UI's reactive mirror. Call on creation, adoption or in tests.
pub fn set_current(id: &str) {
    seed();
    if let Ok(mut ids) = registry().write() {
        ids.current = Some(id.to_string());
    }
    notify();
}

/// The archive list as it will be after rotating `olds` out for `new`, for
/// the caller to persist (in the rotation's transaction) before
/// [`apply_rotation`]. Nothing changes here.
pub fn archives_after_rotation(olds: &[&str], new: &str) -> Archives {
    seed();
    let mut ids = registry().read().map(|ids| ids.clone()).unwrap_or_default();
    ids.rotate(olds, new);
    ids.archives
}

/// Record a rotation in memory, once it is persisted.
pub fn apply_rotation(olds: &[&str], new: &str) {
    seed();
    if let Ok(mut ids) = registry().write() {
        ids.rotate(olds, new);
    }
    notify();
}

/// A system session, current or archived: keeps its icon, stays out of
/// projects and is never retitled.
pub fn is_system(id: &str) -> bool {
    read(|ids| ids.is_system(id)).unwrap_or(false)
}

/// The live Hobbes session (not an archive).
pub fn is_current(id: &str) -> bool {
    read(|ids| ids.is_current(id)).unwrap_or(false)
}

/// A rotated-out, read-only archive of a system session.
pub fn is_archive(id: &str) -> bool {
    read(|ids| ids.is_archive(id)).unwrap_or(false)
}

/// Where something addressed to `id` should land: the current session when
/// `id` is an archive, else `id` itself.
pub fn current_for(id: &str) -> String {
    read(|ids| ids.current_for(id)).unwrap_or_else(|| id.to_string())
}

/// Every id the lineage has had (archives oldest first, then the current).
pub fn lineage() -> Vec<String> {
    read(|ids| ids.lineage()).unwrap_or_default()
}

/// `id` and, for a system session, every id of its lineage (the sessions
/// it rotated out of, or its successors when `id` is an archive).
pub fn lineage_of(id: &str) -> Vec<String> {
    let mut ids = if is_system(id) { lineage() } else { Vec::new() };
    if !ids.iter().any(|i| i == id) {
        ids.push(id.to_string());
    }
    ids
}

/// Put a hydrated system session back in shape: its icon emoji (journaled,
/// so a rebuild from the event journal keeps it) and no project tag.
/// Returns whether anything changed (the caller saves).
pub fn repair(
    session_state: &mut dioxus::prelude::Signal<crate::session::SessionState>,
    id: &str,
    emoji: &str,
    home_profile: Option<&str>,
) -> bool {
    use dioxus::prelude::{Readable, Writable};
    use crate::session_events::{log_event, SessionEvent};
    let (needs_emoji, tagged, rehome) = match session_state.peek().sessions.get(id) {
        Some(s) => (
            s.tab_emoji.as_deref() != Some(emoji),
            s.project_id.is_some(),
            home_profile.filter(|h| s.composio_profile.as_deref() != Some(*h)).map(str::to_string),
        ),
        None => return false,
    };
    if !needs_emoji && !tagged && rehome.is_none() {
        return false;
    }
    {
        let mut state = session_state.write();
        if let Some(s) = state.sessions.get_mut(id) {
            if needs_emoji {
                s.tab_emoji = Some(emoji.to_string());
            }
            if tagged {
                s.project_id = None;
                s.project_tag_user_set = false;
            }
            if let Some(home) = &rehome {
                s.composio_profile = Some(home.clone());
            }
        }
    }
    if let Some(home) = rehome {
        tracing::info!(session_id = id, "system session: moved to Hobbes' home profile");
        log_event(id, SessionEvent::ComposioProfileSet { profile: Some(home) });
    }
    if needs_emoji {
        log_event(id, SessionEvent::TabTitled { emoji: Some(emoji.to_string()), title: None });
    }
    if tagged {
        tracing::info!(session_id = id, "system session: removed its project tag");
        log_event(id, SessionEvent::ProjectTagged { project_id: None, user_set: false });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_sessions_are_recognised() {
        set_current("test-hobbes-session");
        assert!(is_system("test-hobbes-session"));
        assert!(is_current("test-hobbes-session"));
        assert!(!is_system("an-ordinary-chat"));
    }

    #[test]
    fn lineage_routes_archives_to_the_current_session() {
        let mut ids = Ids { current: Some("h-1".into()), ..Default::default() };
        assert_eq!(ids.current_for("h-1"), "h-1");
        assert!(!ids.is_archive("h-1"));

        ids.rotate(&["h-1"], "h-2");
        ids.rotate(&["h-2"], "h-3");
        // Archives keep their kind (icon, no project, no retitle) ...
        assert!(ids.is_system("h-1"));
        assert!(ids.is_archive("h-1") && ids.is_archive("h-2"));
        assert!(!ids.is_current("h-1") && ids.is_current("h-3"));
        // ... and whatever is addressed to them lands in the current one.
        assert_eq!(ids.current_for("h-1"), "h-3");
        assert_eq!(ids.current_for("h-2"), "h-3");
        assert_eq!(ids.current_for("h-3"), "h-3");
        assert_eq!(ids.lineage(), vec!["h-1", "h-2", "h-3"]);
        // Ordinary chats pass through untouched.
        assert_eq!(ids.current_for("a-chat"), "a-chat");
        assert!(!ids.is_system(""));
    }

    #[test]
    fn the_pre_merge_sessions_route_to_the_hobbes_session() {
        // The old Heartbeat and Voice sessions, each with an archive of its
        // own, merge into one Hobbes session.
        let stored: Archives =
            serde_json::from_str(r#"{"heartbeat":["hb-0"],"voice":["v-0"],"successors":{"hb-0":"hb-1","v-0":"v-1"}}"#)
                .unwrap();
        let archives = stored.normalize();
        assert_eq!(archives.hobbes, vec!["hb-0", "v-0"]);
        let mut ids = Ids { archives, ..Default::default() };
        ids.rotate(&["hb-1", "v-1"], "hobbes");
        assert_eq!(ids.lineage(), vec!["hb-0", "v-0", "hb-1", "v-1", "hobbes"]);
        for old in ["hb-0", "v-0", "hb-1", "v-1"] {
            assert!(ids.is_archive(old), "{old}");
            assert_eq!(ids.current_for(old), "hobbes");
            assert_eq!(ids.archives.resolve(old), "hobbes");
        }
        // The legacy lists are never written back.
        let json = serde_json::to_value(&ids.archives).unwrap();
        assert!(json.get("heartbeat").is_none() && json.get("voice").is_none());
        let back = Archives::parse(&json.to_string()).unwrap();
        assert_eq!(back.hobbes, ids.archives.hobbes);
    }

    #[test]
    fn rotation_is_idempotent_and_never_archives_the_current() {
        let mut ids = Ids::default();
        ids.rotate(&["h-1"], "h-2");
        ids.rotate(&["h-1"], "h-2");
        assert_eq!(ids.archives.hobbes, vec!["h-1"]);
        ids.rotate(&["h-2"], "h-2");
        assert_eq!(ids.archives.hobbes, vec!["h-1"]);
        // An archive list that (after a crash mid-rotation) still names the
        // current id treats it as current, not archived.
        ids.archives.hobbes.push("h-2".into());
        assert!(!ids.is_archive("h-2"));
        assert_eq!(ids.current_for("h-2"), "h-2");
        assert_eq!(ids.lineage(), vec!["h-1", "h-2"]);
        let json = serde_json::to_string(&ids.archives).unwrap();
        assert_eq!(Archives::parse(&json).unwrap(), ids.archives);
    }

    #[test]
    fn a_pointer_left_on_an_archive_resolves_forward() {
        let mut ids = Ids::default();
        ids.rotate(&["h-1"], "h-2");
        ids.rotate(&["h-2"], "h-3");
        let a = &ids.archives;
        assert_eq!(a.resolve("h-1"), "h-3");
        assert_eq!(a.resolve("h-2"), "h-3");
        assert_eq!(a.resolve("h-3"), "h-3");
        assert_eq!(a.resolve("a-chat"), "a-chat");
        let mut cyclic = Archives::default();
        cyclic.successors.insert("x".into(), "y".into());
        cyclic.successors.insert("y".into(), "x".into());
        let _ = cyclic.resolve("x");
    }

    #[test]
    fn set_current_and_rotation_reach_the_registry_and_the_reader() {
        use dioxus::prelude::*;
        // Without a runtime the registry still updates.
        set_current("h-1");
        assert!(is_system("h-1") && is_current("h-1"));
        assert_eq!(current().as_deref(), Some("h-1"));
        let mut dom = VirtualDom::new(|| {
            set_current("h-1");
            assert_eq!(current_reactive().as_deref(), Some("h-1"));
            apply_rotation(&["h-1"], "h-2");
            assert_eq!(current().as_deref(), Some("h-2"));
            assert_eq!(current_reactive().as_deref(), Some("h-2"));
            assert!(is_current("h-2") && is_system("h-2"));
            assert!(is_archive("h-1") && is_system("h-1"));
            assert_eq!(current_for("h-1"), "h-2");
            set_current("h-3");
            assert_eq!(current_reactive().as_deref(), Some("h-3"));
            rsx! { div {} }
        });
        crate::test_support::rebuild_or_fail(&mut dom);
    }
}
