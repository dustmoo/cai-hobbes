//! System sessions: the chat sessions Hobbes runs for itself — the
//! Heartbeat and Voice tabs. They are a fixed part of the app, not topics:
//! they keep their name and icon (drawn from the session id, so a lost or
//! changed emoji can't hide them), are never filed under a project, and are
//! never retitled from a summary.
//!
//! A system session that grows too large is **rotated**
//! (`session_rotation`): a fresh session takes over and the old one stays in
//! History as a read-only archive. The registry keeps each kind's lineage —
//! the current id plus every archived one — so archives keep their icon and
//! their exemptions, and anything still addressed to an archived id (a
//! dispatch result, a relay, a queued message) is routed to the current
//! session through [`current_for`].
//!
//! A plain registry (no Signals) so background code and tests can ask too.
//! Seeded on first use from the ids the two features persist (and the
//! archive list, meta key [`ARCHIVES_META_KEY`]), and updated whenever
//! either creates or rotates its session.

use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

/// `meta` key holding the archived ids of each kind (JSON [`Archives`]).
pub const ARCHIVES_META_KEY: &str = "system_session_archives";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemSession {
    Heartbeat,
    Voice,
}

impl SystemSession {
    pub const ALL: [SystemSession; 2] = [SystemSession::Heartbeat, SystemSession::Voice];

    /// The session's fixed title.
    pub fn title(self) -> &'static str {
        match self {
            SystemSession::Heartbeat => crate::heartbeat::runtime::SESSION_TITLE,
            SystemSession::Voice => crate::voice::session::SESSION_TITLE,
        }
    }
}

/// Archived (rotated-out) ids per kind, oldest first. Persisted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Archives {
    #[serde(default)]
    pub heartbeat: Vec<String>,
    #[serde(default)]
    pub voice: Vec<String>,
    /// Archived id → the session that replaced it. Written in the same
    /// transaction as the rows and the feature's pointer, so a pointer that
    /// a late, older write set back to an archive is followed forward again
    /// at the next load ([`resolve_successor`]).
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

    fn of(&self, kind: SystemSession) -> &Vec<String> {
        match kind {
            SystemSession::Heartbeat => &self.heartbeat,
            SystemSession::Voice => &self.voice,
        }
    }

    fn of_mut(&mut self, kind: SystemSession) -> &mut Vec<String> {
        match kind {
            SystemSession::Heartbeat => &mut self.heartbeat,
            SystemSession::Voice => &mut self.voice,
        }
    }
}

/// The registry's contents: each kind's current id and its archives. Pure,
/// so the lineage rules are unit-tested on a local value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ids {
    pub heartbeat: Option<String>,
    pub voice: Option<String>,
    pub archives: Archives,
    /// The persisted ids have been read (the store was open).
    seeded: bool,
}

impl Ids {
    fn current(&self, kind: SystemSession) -> Option<&str> {
        match kind {
            SystemSession::Heartbeat => self.heartbeat.as_deref(),
            SystemSession::Voice => self.voice.as_deref(),
        }
    }

    fn set_current(&mut self, kind: SystemSession, id: &str) {
        let slot = match kind {
            SystemSession::Heartbeat => &mut self.heartbeat,
            SystemSession::Voice => &mut self.voice,
        };
        *slot = Some(id.to_string());
    }

    /// Which system session `id` is — current or archived.
    pub fn kind(&self, id: &str) -> Option<SystemSession> {
        if id.is_empty() {
            return None;
        }
        SystemSession::ALL.into_iter().find(|&k| {
            self.current(k) == Some(id) || self.archives.of(k).iter().any(|a| a == id)
        })
    }

    /// `id` is a rotated-out archive (and not, by some race, also current).
    pub fn is_archive(&self, id: &str) -> bool {
        self.kind(id).is_some_and(|k| self.current(k) != Some(id))
    }

    /// `id` is the live session of its kind.
    pub fn is_current(&self, id: &str) -> bool {
        SystemSession::ALL.into_iter().any(|k| self.current(k) == Some(id))
    }

    /// Where something addressed to `id` should land: the current session of
    /// its kind when `id` is an archive, else `id` itself.
    pub fn current_for(&self, id: &str) -> String {
        match self.kind(id) {
            Some(k) if self.is_archive(id) => self.current(k).unwrap_or(id).to_string(),
            _ => id.to_string(),
        }
    }

    /// Every id the kind has had: archives (oldest first), then the current.
    pub fn lineage(&self, kind: SystemSession) -> Vec<String> {
        let mut out: Vec<String> =
            self.archives.of(kind).iter().filter(|a| self.current(kind) != Some(a.as_str())).cloned().collect();
        if let Some(c) = self.current(kind) {
            out.push(c.to_string());
        }
        out
    }

    /// The newest archive of the kind, if any.
    pub fn latest_archive(&self, kind: SystemSession) -> Option<&str> {
        self.archives.of(kind).iter().rev().find(|a| self.current(kind) != Some(a.as_str())).map(String::as_str)
    }

    /// Rotate: `old` joins the archives, `new` becomes current.
    pub fn rotate(&mut self, kind: SystemSession, old: &str, new: &str) {
        if !old.is_empty() && old != new {
            let archives = self.archives.of_mut(kind);
            if !archives.iter().any(|a| a == old) {
                archives.push(old.to_string());
            }
            self.archives.successors.insert(old.to_string(), new.to_string());
        }
        self.set_current(kind, new);
    }
}

fn registry() -> &'static RwLock<Ids> {
    static REG: OnceLock<RwLock<Ids>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(Ids::default()))
}

/// Fill unknown ids from the store, once it is open.
fn seed() {
    if registry().read().map(|ids| ids.seeded).unwrap_or(true) || !crate::session_store::is_ready() {
        return;
    }
    let heartbeat = crate::session_store::meta_get(crate::heartbeat::META_KEY)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .and_then(|v| v.get("session_id").and_then(|s| s.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    let voice = crate::session_store::meta_get(crate::voice::session::META_KEY).filter(|s| !s.is_empty());
    let archives: Archives = crate::session_store::meta_get(ARCHIVES_META_KEY)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    let heartbeat = heartbeat.map(|id| archives.resolve(&id));
    let voice = voice.map(|id| archives.resolve(&id));
    if let Ok(mut ids) = registry().write() {
        if ids.heartbeat.is_none() {
            ids.heartbeat = heartbeat;
        }
        if ids.voice.is_none() {
            ids.voice = voice;
        }
        for (old, new) in &archives.successors {
            ids.archives.successors.entry(old.clone()).or_insert_with(|| new.clone());
        }
        for kind in SystemSession::ALL {
            for a in archives.of(kind) {
                if !ids.archives.of(kind).contains(a) {
                    ids.archives.of_mut(kind).push(a.clone());
                }
            }
        }
        ids.seeded = true;
    }
}

fn read<T>(f: impl FnOnce(&Ids) -> T) -> Option<T> {
    seed();
    registry().read().ok().map(|ids| f(&ids))
}

/// Record the session a system feature uses (on creation or adoption).
pub fn register(kind: SystemSession, id: &str) {
    if let Ok(mut ids) = registry().write() {
        ids.set_current(kind, id);
    }
}

/// The archive list as it will be after rotating `old` out for `new`, for
/// the caller to persist (in the rotation's transaction) before
/// [`apply_rotation`]. Nothing changes here.
pub fn archives_after_rotation(kind: SystemSession, old: &str, new: &str) -> Archives {
    seed();
    let mut ids = registry().read().map(|ids| ids.clone()).unwrap_or_default();
    ids.rotate(kind, old, new);
    ids.archives
}

/// Record a rotation in memory, once it is persisted.
pub fn apply_rotation(kind: SystemSession, old: &str, new: &str) {
    seed();
    if let Ok(mut ids) = registry().write() {
        ids.rotate(kind, old, new);
    }
}

/// Follow a persisted pointer forward through recorded rotations: a feature
/// loading its session id calls this, so a pointer left on an archive (a
/// crash between the rotation's commit and a stale async save) recovers.
pub fn resolve_successor(id: &str) -> String {
    crate::session_store::meta_get(ARCHIVES_META_KEY)
        .and_then(|json| serde_json::from_str::<Archives>(&json).ok())
        .map(|a| a.resolve(id))
        .unwrap_or_else(|| id.to_string())
}

/// Which system session `id` is (current or archived), if any.
pub fn kind(id: &str) -> Option<SystemSession> {
    read(|ids| ids.kind(id)).flatten()
}

/// A system session, current or archived: keeps its icon, stays out of
/// projects and is never retitled.
pub fn is_system(id: &str) -> bool {
    kind(id).is_some()
}

/// The live Heartbeat or Voice session (not an archive).
pub fn is_current(id: &str) -> bool {
    read(|ids| ids.is_current(id)).unwrap_or(false)
}

/// A rotated-out, read-only archive of a system session.
pub fn is_archive(id: &str) -> bool {
    read(|ids| ids.is_archive(id)).unwrap_or(false)
}

/// Where something addressed to `id` should land: the current session of
/// its kind when `id` is an archive, else `id` itself.
pub fn current_for(id: &str) -> String {
    read(|ids| ids.current_for(id)).unwrap_or_else(|| id.to_string())
}

/// Every id `kind` has had (archives oldest first, then the current one).
pub fn lineage(kind: SystemSession) -> Vec<String> {
    read(|ids| ids.lineage(kind)).unwrap_or_default()
}

/// The newest archive of `kind`, if any.
pub fn latest_archive(kind: SystemSession) -> Option<String> {
    read(|ids| ids.latest_archive(kind).map(str::to_string)).flatten()
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
        register(SystemSession::Voice, "test-voice-session");
        register(SystemSession::Heartbeat, "test-heartbeat-session");
        assert_eq!(kind("test-voice-session"), Some(SystemSession::Voice));
        assert_eq!(kind("test-heartbeat-session"), Some(SystemSession::Heartbeat));
        assert!(!is_system("an-ordinary-chat"));
    }

    #[test]
    fn lineage_routes_archives_to_the_current_session() {
        let mut ids = Ids { heartbeat: Some("hb-1".into()), voice: Some("v-1".into()), ..Default::default() };
        assert_eq!(ids.current_for("hb-1"), "hb-1");
        assert!(!ids.is_archive("hb-1"));

        ids.rotate(SystemSession::Heartbeat, "hb-1", "hb-2");
        ids.rotate(SystemSession::Heartbeat, "hb-2", "hb-3");
        // Archives keep their kind (icon, no project, no retitle) ...
        assert_eq!(ids.kind("hb-1"), Some(SystemSession::Heartbeat));
        assert_eq!(ids.kind("hb-2"), Some(SystemSession::Heartbeat));
        assert!(ids.is_archive("hb-1") && ids.is_archive("hb-2"));
        assert!(!ids.is_current("hb-1") && ids.is_current("hb-3"));
        // ... and whatever is addressed to them lands in the current one.
        assert_eq!(ids.current_for("hb-1"), "hb-3");
        assert_eq!(ids.current_for("hb-2"), "hb-3");
        assert_eq!(ids.current_for("hb-3"), "hb-3");
        assert_eq!(ids.lineage(SystemSession::Heartbeat), vec!["hb-1", "hb-2", "hb-3"]);
        assert_eq!(ids.latest_archive(SystemSession::Heartbeat), Some("hb-2"));

        // Kinds don't mix; ordinary chats pass through untouched.
        assert_eq!(ids.current_for("v-1"), "v-1");
        assert_eq!(ids.latest_archive(SystemSession::Voice), None);
        assert_eq!(ids.current_for("a-chat"), "a-chat");
        assert_eq!(ids.kind(""), None);
        ids.rotate(SystemSession::Voice, "v-1", "v-2");
        assert_eq!(ids.current_for("v-1"), "v-2");
        assert_eq!(ids.kind("v-1"), Some(SystemSession::Voice));
    }

    #[test]
    fn rotation_is_idempotent_and_never_archives_the_current() {
        let mut ids = Ids::default();
        ids.rotate(SystemSession::Heartbeat, "hb-1", "hb-2");
        ids.rotate(SystemSession::Heartbeat, "hb-1", "hb-2");
        assert_eq!(ids.archives.heartbeat, vec!["hb-1"]);
        ids.rotate(SystemSession::Heartbeat, "hb-2", "hb-2");
        assert_eq!(ids.archives.heartbeat, vec!["hb-1"]);
        // An archive list that (after a crash mid-rotation) still names the
        // current id treats it as current, not archived.
        ids.archives.heartbeat.push("hb-2".into());
        assert!(!ids.is_archive("hb-2"));
        assert_eq!(ids.current_for("hb-2"), "hb-2");
        assert_eq!(ids.lineage(SystemSession::Heartbeat), vec!["hb-1", "hb-2"]);
        // Archives round-trip, and an old blob without a kind still loads.
        let json = serde_json::to_string(&ids.archives).unwrap();
        assert_eq!(serde_json::from_str::<Archives>(&json).unwrap(), ids.archives);
        let partial: Archives = serde_json::from_str(r#"{"voice":["v-0"]}"#).unwrap();
        assert!(partial.heartbeat.is_empty());
    }

    #[test]
    fn a_pointer_left_on_an_archive_resolves_forward() {
        let mut ids = Ids::default();
        ids.rotate(SystemSession::Heartbeat, "hb-1", "hb-2");
        ids.rotate(SystemSession::Heartbeat, "hb-2", "hb-3");
        let a = &ids.archives;
        // A late, older save set the Heartbeat's pointer back to hb-1 (or
        // hb-2) after the rotation committed: loading follows it forward.
        assert_eq!(a.resolve("hb-1"), "hb-3");
        assert_eq!(a.resolve("hb-2"), "hb-3");
        assert_eq!(a.resolve("hb-3"), "hb-3");
        assert_eq!(a.resolve("a-chat"), "a-chat");
        // The map survives the round trip; a cycle can't spin.
        let back: Archives = serde_json::from_str(&serde_json::to_string(a).unwrap()).unwrap();
        assert_eq!(back.resolve("hb-1"), "hb-3");
        let mut cyclic = Archives::default();
        cyclic.successors.insert("x".into(), "y".into());
        cyclic.successors.insert("y".into(), "x".into());
        let _ = cyclic.resolve("x");
    }
}
