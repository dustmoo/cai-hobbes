//! Logging hygiene helpers.
//!
//! Everything that ends up in `hobbes.log` must be safe to attach to a bug
//! report: no API keys, no OAuth tokens, no ICS feed URLs, no full chat text.
//! These helpers are the single place that knows how to make a value
//! log-safe, plus the small once / rate-limit gates used to keep repetitive
//! warnings from flooding the file.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Network errors and URLs ─────────────────────────────────────────────────

/// Render a `reqwest::Error` without the request URL.
///
/// `reqwest::Error`'s `Display` appends ` for url (...)`, which carries query
/// strings (API keys, ICS feed tokens, `user_id`). Use this at every
/// `.send()` / `map_err` whose error is logged or shown to the user.
pub fn net_err(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// Reduce a URL to `scheme://host[:port]` for logging.
///
/// Strips userinfo, path, query string and fragment. Input that does not look
/// like a URL is replaced with a placeholder rather than echoed, since it may
/// be a secret pasted into the wrong field.
pub fn redact_url(url: &str) -> String {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<non-url>".to_string();
    };
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
        return "<non-url>".to_string();
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // Drop `user:pass@`.
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if host.is_empty() {
        return format!("{}://<no-host>", scheme);
    }
    format!("{}://{}", scheme, host)
}

/// Whether full request/prompt/response dumps to the temp dir are allowed.
/// Only with an explicit `HOBBES_DEBUG_DUMPS=1` — never implied by the log
/// level, since those files contain chat text, tool payloads and auth configs.
pub fn debug_dumps_enabled() -> bool {
    debug_dumps_enabled_from(std::env::var("HOBBES_DEBUG_DUMPS").ok().as_deref())
}

fn debug_dumps_enabled_from(v: Option<&str>) -> bool {
    v.map(str::trim) == Some("1")
}

// ── Truncation ──────────────────────────────────────────────────────────────

/// Default cap for response bodies logged at WARN/ERROR.
pub const LOG_BODY_MAX_CHARS: usize = 300;

/// Truncate `s` to at most `max_chars` characters for a log line.
///
/// Multibyte-safe (counts chars, never splits a UTF-8 sequence). When text is
/// dropped, a `…[+N chars]` marker is appended so the reader knows.
pub fn truncate_for_log(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s.to_string(),
        Some((byte_idx, _)) => {
            let dropped = s[byte_idx..].chars().count();
            format!("{}…[+{} chars]", &s[..byte_idx], dropped)
        }
    }
}

/// Join up to `max` names, then `and N more`.
pub fn cap_list<S: AsRef<str>>(names: &[S], max: usize) -> String {
    let shown: Vec<&str> = names.iter().take(max).map(|s| s.as_ref()).collect();
    let mut out = shown.join(", ");
    if names.len() > max {
        out.push_str(&format!(" and {} more", names.len() - max));
    }
    out
}

// ── Once / rate-limit gates ─────────────────────────────────────────────────

fn once_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Returns `true` the first time it is called with `key` in this process,
/// `false` afterwards. Use to log a condition at INFO/WARN once, then DEBUG.
pub fn first_time(key: &str) -> bool {
    match once_set().lock() {
        Ok(mut set) => set.insert(key.to_string()),
        // A poisoned lock only means another thread panicked mid-insert;
        // erring on the side of logging is fine.
        Err(_) => true,
    }
}

fn rate_map() -> &'static Mutex<HashMap<String, Instant>> {
    static MAP: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns `true` if `key` has not been allowed within the last `window`
/// (and records now as its last allowed time), `false` otherwise.
pub fn allow_every(key: &str, window: Duration) -> bool {
    allow_every_at(key, window, Instant::now())
}

fn allow_every_at(key: &str, window: Duration, now: Instant) -> bool {
    let Ok(mut map) = rate_map().lock() else { return true };
    match map.get(key) {
        Some(last) if now.saturating_duration_since(*last) < window => false,
        _ => {
            map.insert(key.to_string(), now);
            true
        }
    }
}

// ── Composio profile change description ─────────────────────────────────────

/// The Composio profile properties whose change forces a client reinit.
/// Order: name, api_key, base_url, entity_id, user_id.
pub type ProfileSignature = [String; 5];

const PROFILE_FIELD_NAMES: [&str; 5] = ["name", "api_key", "base_url", "entity_id", "user_id"];

/// Describe a Composio profile change without any field values except the
/// profile name. Never prints the API key, user id or entity id.
pub fn describe_profile_change(prev: Option<&ProfileSignature>, cur: Option<&ProfileSignature>) -> String {
    match (prev, cur) {
        (_, None) => "no active profile".to_string(),
        (None, Some(c)) => format!("profile '{}' (initial load)", c[0]),
        (Some(p), Some(c)) => {
            let changed: Vec<&str> = PROFILE_FIELD_NAMES
                .iter()
                .enumerate()
                .filter(|(i, _)| p[*i] != c[*i])
                .map(|(_, n)| *n)
                .collect();
            format!("profile '{}', changed fields: [{}]", c[0], changed.join(", "))
        }
    }
}

// ── Log filter selection ────────────────────────────────────────────────────

/// This crate's tracing target root. Event targets are module paths, so the
/// root is the crate name: `Hobbes` (capital H) for the main package, but
/// `windows_app` / `macos_app` when the shared `main.rs` is compiled by the
/// per-platform app crates. A hard-coded `hobbes=...` matches nothing.
pub const CRATE_TARGET: &str = env!("CARGO_CRATE_NAME");

/// Release-build default filter.
#[cfg_attr(debug_assertions, allow(dead_code))] // used by the release init_logger
pub fn default_release_filter() -> String {
    format!(
        "warn,{}=info,rmcp=warn,h2=error,hyper=error,hyper_util=error,rustls=error",
        CRATE_TARGET
    )
}

/// Debug-build default filter.
#[cfg_attr(not(debug_assertions), allow(dead_code))] // used by the debug init_logger
pub fn default_debug_filter() -> String {
    format!("info,{}=debug", CRATE_TARGET)
}

/// Choose the effective filter string from the `HOBBES_LOG` override.
///
/// Returns `(filter, warning)`; `warning` is set when the override was present
/// but failed to parse, in which case `default` is used.
pub fn choose_log_filter(env_value: Option<&str>, default: &str) -> (String, Option<String>) {
    match env_value.map(str::trim).filter(|v| !v.is_empty()) {
        None => (default.to_string(), None),
        Some(v) => match tracing_subscriber::EnvFilter::builder().parse(v) {
            Ok(_) => (v.to_string(), None),
            Err(e) => (
                default.to_string(),
                Some(format!("HOBBES_LOG={:?} is not a valid filter ({}); using default", v, e)),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_strips_path_query_and_key() {
        let out = redact_url("https://generativelanguage.googleapis.com/v1beta/models/x:generateContent?key=AIzaSECRET&alt=sse");
        assert_eq!(out, "https://generativelanguage.googleapis.com");
        assert!(!out.contains("AIza"));
        assert!(!out.contains('?'));
    }

    #[test]
    fn redact_url_strips_userinfo_and_keeps_port() {
        assert_eq!(redact_url("http://user:pw@localhost:8080/sse?user_id=bob"), "http://localhost:8080");
        assert_eq!(redact_url("https://backend.composio.dev?api_key=sk"), "https://backend.composio.dev");
        assert_eq!(redact_url("https://x.dev#frag"), "https://x.dev");
    }

    #[test]
    fn redact_url_rejects_non_urls() {
        assert_eq!(redact_url("sk-ant-secret"), "<non-url>");
        assert_eq!(redact_url(""), "<non-url>");
        assert_eq!(redact_url("https:///path?key=1"), "https://<no-host>");
    }

    #[tokio::test]
    async fn net_err_omits_url() {
        // Connection to a closed local port fails fast; the error must not
        // carry the query string.
        let client = reqwest::Client::new();
        let err = client
            .get("http://127.0.0.1:1/feed.ics?token=SECRET123")
            .send()
            .await
            .expect_err("port 1 should refuse");
        assert!(err.to_string().contains("SECRET123"), "precondition: reqwest includes the url");
        let msg = net_err(err);
        assert!(!msg.contains("SECRET123"), "got: {msg}");
        assert!(!msg.contains("127.0.0.1:1/feed"), "got: {msg}");
    }

    #[test]
    fn debug_dumps_need_explicit_opt_in() {
        assert!(!debug_dumps_enabled_from(None));
        assert!(!debug_dumps_enabled_from(Some("")));
        assert!(!debug_dumps_enabled_from(Some("true")));
        assert!(!debug_dumps_enabled_from(Some("0")));
        assert!(debug_dumps_enabled_from(Some("1")));
    }

    #[test]
    fn truncate_for_log_is_multibyte_safe() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
        assert_eq!(truncate_for_log("hello", 5), "hello");
        assert_eq!(truncate_for_log("héllo wörld", 4), "héll…[+7 chars]");
        let emoji = "🦀🦀🦀🦀";
        assert_eq!(truncate_for_log(emoji, 2), "🦀🦀…[+2 chars]");
        assert_eq!(truncate_for_log("", 0), "");
        assert_eq!(truncate_for_log("ab", 0), "…[+2 chars]");
    }

    #[test]
    fn cap_list_caps_at_max() {
        let names = ["a", "b", "c", "d", "e", "f", "g"];
        assert_eq!(cap_list(&names, 5), "a, b, c, d, e and 2 more");
        assert_eq!(cap_list(&names[..3], 5), "a, b, c");
        let empty: [&str; 0] = [];
        assert_eq!(cap_list(&empty, 5), "");
    }

    #[test]
    fn first_time_fires_once_per_key() {
        assert!(first_time("log_utils_test_key_a"));
        assert!(!first_time("log_utils_test_key_a"));
        assert!(first_time("log_utils_test_key_b"));
    }

    #[test]
    fn allow_every_respects_window() {
        let t0 = Instant::now();
        let w = Duration::from_secs(600);
        assert!(allow_every_at("log_utils_rate_test", w, t0));
        assert!(!allow_every_at("log_utils_rate_test", w, t0 + Duration::from_secs(10)));
        assert!(allow_every_at("log_utils_rate_test", w, t0 + Duration::from_secs(601)));
        assert!(allow_every_at("log_utils_rate_test_other", w, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn describe_profile_change_never_prints_secrets() {
        let prev: ProfileSignature = [
            "Work".into(), "ak_OLDSECRET".into(), "https://backend.composio.dev".into(), "ent_1".into(), "user_1".into(),
        ];
        let cur: ProfileSignature = [
            "Work".into(), "ak_NEWSECRET".into(), "https://backend.composio.dev".into(), "ent_1".into(), "user_2".into(),
        ];
        let out = describe_profile_change(Some(&prev), Some(&cur));
        assert_eq!(out, "profile 'Work', changed fields: [api_key, user_id]");
        for secret in ["SECRET", "user_2", "user_1", "ent_1", "ak_"] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
        let initial = describe_profile_change(None, Some(&cur));
        assert!(!initial.contains("SECRET") && !initial.contains("user_2"));
        assert_eq!(describe_profile_change(Some(&prev), None), "no active profile");
    }

    #[test]
    fn choose_log_filter_handles_override_and_bad_values() {
        let def = default_release_filter();
        assert_eq!(choose_log_filter(None, &def), (def.clone(), None));
        assert_eq!(choose_log_filter(Some("  "), &def), (def.clone(), None));
        assert_eq!(choose_log_filter(Some("Hobbes=trace"), &def), ("Hobbes=trace".to_string(), None));
        let (f, warn) = choose_log_filter(Some("Hobbes=notalevel[[["), &def);
        assert_eq!(f, def);
        assert!(warn.is_some());
    }

    #[test]
    fn default_filters_parse_and_target_this_crate() {
        assert_eq!(CRATE_TARGET, "Hobbes");
        assert_eq!(
            default_release_filter(),
            "warn,Hobbes=info,rmcp=warn,h2=error,hyper=error,hyper_util=error,rustls=error"
        );
        assert_eq!(default_debug_filter(), "info,Hobbes=debug");
        for f in [default_release_filter(), default_debug_filter()] {
            assert!(tracing_subscriber::EnvFilter::builder().parse(&f).is_ok(), "{f}");
        }
    }
}
