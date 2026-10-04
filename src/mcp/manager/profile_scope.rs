//! Which Composio profile an MCP context is built for, and how a shared
//! (global) context is scoped down to one session's profile.
//!
//! Background refreshes (a server finishing its connect, a marketplace toggle,
//! a retry) rebuild the shared `Signal<McpContext>`. They used to pass no
//! profile, which with several Composio profiles connected put every
//! profile's native server in the context and made `resolve_composio_key`
//! refuse (correctly) to pick one. These helpers give every refresh a profile
//! and keep a session pinned to a different profile from inheriting the
//! global profile's Composio tools.

use super::{is_composio_native, McpContext, McpManager, COMPOSIO_NATIVE_PREFIX};
use crate::session::SessionState;
use crate::settings::Settings;
use dioxus_signals::Readable;

/// The Composio profile id an MCP context should be built for.
///
/// A session's own pin wins; otherwise the global active profile (validated
/// against the configured profiles, as `get_active_profile` does). `None`
/// only when neither exists. Same rule the send path uses
/// (`fetch_fresh_mcp_context` in `components/chat.rs`).
pub fn resolve_context_profile(session_pin: Option<&str>, settings: &Settings) -> Option<String> {
    session_pin
        .map(str::to_string)
        .or_else(|| settings.get_active_profile().map(|p| p.id.clone()))
}

/// `resolve_context_profile` for a session looked up by id. An unknown or
/// non-hydrated session falls back to the global active profile.
pub fn context_profile_for_session(
    state: &SessionState,
    session_id: &str,
    settings: &Settings,
) -> Option<String> {
    let pin = state
        .sessions
        .get(session_id)
        .and_then(|s| s.composio_profile.as_deref());
    resolve_context_profile(pin, settings)
}

impl McpManager {
    /// Profile the manager's own background refreshes build the shared
    /// context for: the global active profile. Sessions pinned elsewhere are
    /// protected by `scope_context_to_profile` in the ChatWindow sync.
    pub(crate) fn background_context_profile(&self) -> Option<String> {
        resolve_context_profile(None, &self.settings.peek())
    }

    /// Rebuild the shared context for the global active profile. Use this for
    /// every background refresh instead of `get_mcp_context(None)`.
    pub async fn get_active_profile_mcp_context(&self) -> McpContext {
        let profile_id = self.background_context_profile();
        self.get_mcp_context(profile_id).await
    }
}

/// Whether a Composio-native server entry in a context belongs to the given
/// profile. Context entries are named `composio-native:{profile name}` (main
/// init path) or `composio-native:{profile id}` (stub/reload paths), so both
/// are accepted. The bare legacy singleton belongs to any profile, mirroring
/// `get_mcp_context`'s filter; with no profile only the singleton belongs.
fn native_entry_belongs(name: &str, profile_id: Option<&str>, profile_name: Option<&str>) -> bool {
    if name == COMPOSIO_NATIVE_PREFIX {
        return true;
    }
    let Some(pid) = profile_id else {
        return false;
    };
    let suffix = name
        .strip_prefix(COMPOSIO_NATIVE_PREFIX)
        .and_then(|s| s.strip_prefix(':'))
        .unwrap_or_default();
    suffix == pid || profile_name.is_some_and(|n| suffix == n)
}

/// Scope the shared context to one session's Composio profile.
///
/// Non-Composio servers always come from `global`. If `global` holds no
/// Composio-native entry belonging to another profile, it was built for this
/// profile (or carries no Composio at all) and is used as is. Otherwise it was
/// built for a different profile (a session pinned away from the global
/// active one) or for no profile: this profile's native entries are taken
/// from `global` when present, else kept from `previous` (the session's
/// current view), and `connected_toolkit_slugs` are kept from `previous`,
/// since `global`'s belong to the other profile.
pub fn scope_context_to_profile(
    global: &McpContext,
    previous: Option<&McpContext>,
    profile_id: Option<&str>,
    profile_name: Option<&str>,
) -> McpContext {
    let belongs = |name: &str| native_entry_belongs(name, profile_id, profile_name);
    let foreign = global
        .servers
        .iter()
        .any(|s| is_composio_native(&s.name) && !belongs(&s.name));
    if !foreign {
        return global.clone();
    }

    let own_from_global: Vec<_> = global
        .servers
        .iter()
        .filter(|s| is_composio_native(&s.name) && belongs(&s.name))
        .cloned()
        .collect();
    let own_native = if !own_from_global.is_empty() {
        own_from_global
    } else {
        previous
            .map(|p| {
                p.servers
                    .iter()
                    .filter(|s| is_composio_native(&s.name) && belongs(&s.name))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut servers: Vec<_> = global
        .servers
        .iter()
        .filter(|s| !is_composio_native(&s.name))
        .cloned()
        .collect();
    servers.extend(own_native);

    McpContext {
        servers,
        connected_toolkit_slugs: previous
            .map(|p| p.connected_toolkit_slugs.clone())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::manager::McpServerContext;
    use crate::settings::ComposioProfile;

    fn settings_with(profiles: &[(&str, &str)], active: Option<&str>) -> Settings {
        Settings {
            composio_profiles: profiles
                .iter()
                .map(|(id, name)| ComposioProfile {
                    id: id.to_string(),
                    name: name.to_string(),
                    ..ComposioProfile::default()
                })
                .collect(),
            active_composio_profile: active.map(str::to_string),
            ..Settings::default()
        }
    }

    fn server(name: &str) -> McpServerContext {
        McpServerContext {
            name: name.to_string(),
            description: String::new(),
            tools: Vec::new(),
        }
    }

    fn ctx(names: &[&str], slugs: &[&str]) -> McpContext {
        McpContext {
            servers: names.iter().map(|n| server(n)).collect(),
            connected_toolkit_slugs: slugs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn names(c: &McpContext) -> Vec<&str> {
        let mut v: Vec<&str> = c.servers.iter().map(|s| s.name.as_str()).collect();
        v.sort();
        v
    }

    #[test]
    fn session_pin_wins_over_global_active() {
        let s = settings_with(&[("a", "A"), ("b", "B")], Some("a"));
        assert_eq!(resolve_context_profile(Some("b"), &s), Some("b".to_string()));
    }

    #[test]
    fn global_active_used_when_session_has_no_pin() {
        let s = settings_with(&[("a", "A"), ("b", "B")], Some("a"));
        assert_eq!(resolve_context_profile(None, &s), Some("a".to_string()));
    }

    #[test]
    fn none_only_when_nothing_configured() {
        assert_eq!(resolve_context_profile(None, &Settings::default()), None);
        // An active id that names no configured profile is not trusted.
        let dangling = settings_with(&[("a", "A")], Some("gone"));
        assert_eq!(resolve_context_profile(None, &dangling), None);
    }

    #[test]
    fn context_profile_for_session_reads_the_session_pin() {
        let s = settings_with(&[("a", "A"), ("b", "B")], Some("a"));
        let mut state = SessionState::new();
        let sid = state.create_session_raw(Some("b".to_string()));
        assert_eq!(context_profile_for_session(&state, &sid, &s), Some("b".to_string()));
        // Unknown session: global active.
        assert_eq!(
            context_profile_for_session(&state, "no-such-session", &s),
            Some("a".to_string())
        );
    }

    #[test]
    fn pinned_session_keeps_its_profile_when_global_refreshes_for_another() {
        // Global context refreshed for profile A (by name, as the init path names it).
        let global = ctx(&["filesystem", "composio-native:A"], &["gmail"]);
        // Session pinned to B currently shows B's tools.
        let previous = ctx(&["filesystem-old", "composio-native:B"], &["slack"]);

        let scoped = scope_context_to_profile(&global, Some(&previous), Some("b"), Some("B"));

        // Fresh non-Composio servers, B's own Composio entry, never A's.
        assert_eq!(names(&scoped), vec!["composio-native:B", "filesystem"]);
        assert_eq!(scoped.connected_toolkit_slugs, vec!["slack".to_string()]);
    }

    #[test]
    fn matching_profile_context_used_as_is() {
        // Entry named by id (stub path) and by name (init path) both match.
        for entry in ["composio-native:b", "composio-native:B"] {
            let global = ctx(&["filesystem", entry], &["slack", "notion"]);
            let previous = ctx(&["composio-native:B"], &["slack"]);
            let scoped = scope_context_to_profile(&global, Some(&previous), Some("b"), Some("B"));
            assert_eq!(scoped, global);
        }
    }

    #[test]
    fn profileless_context_with_both_profiles_is_scoped() {
        // The pre-fix shape: get_mcp_context(None) with two native clients.
        let global = ctx(&["filesystem", "composio-native:A", "composio-native:B"], &[]);
        let previous = ctx(&["composio-native:B"], &["slack"]);
        let scoped = scope_context_to_profile(&global, Some(&previous), Some("b"), Some("B"));
        assert_eq!(names(&scoped), vec!["composio-native:B", "filesystem"]);
        assert_eq!(scoped.connected_toolkit_slugs, vec!["slack".to_string()]);
    }

    #[test]
    fn no_profile_keeps_only_legacy_singleton() {
        let global = ctx(&["filesystem", COMPOSIO_NATIVE_PREFIX, "composio-native:A"], &[]);
        let scoped = scope_context_to_profile(&global, None, None, None);
        assert_eq!(names(&scoped), vec![COMPOSIO_NATIVE_PREFIX, "filesystem"]);
    }

    /// Background refreshes must go through the profile helper: a bare
    /// `get_mcp_context(None)` rebuilds the shared context with every
    /// profile's Composio server in it.
    #[test]
    fn background_refresh_sites_do_not_build_profileless_context() {
        let needle = concat!("get_mcp_context", "(None)");
        for (file, src) in [
            ("mcp/manager/lifecycle.rs", include_str!("lifecycle.rs")),
            ("mcp/manager/composio.rs", include_str!("composio.rs")),
            ("mcp/manager/composio_toolkits.rs", include_str!("composio_toolkits.rs")),
            ("components/mcp_marketplace.rs", include_str!("../../components/mcp_marketplace.rs")),
            ("components/chat_input.rs", include_str!("../../components/chat_input.rs")),
        ] {
            assert!(!src.contains(needle), "{file} still calls {needle}");
        }
    }
}
