//! Process-wide cache of context-window limits *learned at runtime* from
//! provider "context length exceeded" errors.
//!
//! There is no reliable way to query a context window before sending (the OpenAI
//! `/v1/models` endpoint omits it, and arbitrary OpenAI-compatible servers vary).
//! So the system budgets optimistically from a name table and, when a server
//! rejects an oversized prompt, records the real limit here. Subsequent prompt
//! builds clamp to the learned value, making the budget self-calibrating.
//!
//! Keyed by `(scope, model)` where `scope` is the endpoint URL for
//! OpenAI-compatible servers (whose limits are endpoint-specific) or the provider
//! name for first-party providers.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

static LEARNED_WINDOWS: OnceLock<Mutex<HashMap<(String, String), usize>>> = OnceLock::new();

fn store() -> &'static Mutex<HashMap<(String, String), usize>> {
    LEARNED_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up a previously-learned context window for `(scope, model)`.
pub fn learned_window(scope: &str, model: &str) -> Option<usize> {
    store()
        .lock()
        .ok()?
        .get(&(scope.to_string(), model.to_string()))
        .copied()
}

/// Record a context window learned from a provider error. Keeps the *smallest*
/// value seen for a key, since a rejection is hard evidence of a ceiling and we
/// prefer the most conservative budget once we've been told "too large".
pub fn record_window(scope: &str, model: &str, tokens: usize) {
    if tokens == 0 {
        return;
    }
    if let Ok(mut map) = store().lock() {
        let key = (scope.to_string(), model.to_string());
        map.entry(key)
            .and_modify(|existing| {
                if tokens < *existing {
                    *existing = tokens;
                }
            })
            .or_insert(tokens);
    }
}

/// Clamp a base context window by any learned ceiling for `(scope, model)`.
/// `base` may be `None` (unknown window); a learned value then becomes the window.
pub fn clamp_to_learned(scope: &str, model: &str, base: Option<usize>) -> Option<usize> {
    match (base, learned_window(scope, model)) {
        (Some(b), Some(l)) => Some(b.min(l)),
        (Some(b), None) => Some(b),
        (None, learned) => learned,
    }
}

/// Heuristic: does this (already-formatted) provider error indicate the prompt
/// exceeded the model's context window? Used to decide whether an in-turn retry
/// with a smaller budget is worthwhile. Matches the friendly messages produced by
/// all three connectors as well as common raw API phrasings.
pub fn is_context_overflow(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("prompt too large")
        || m.contains("context length")
        || m.contains("maximum context")
        || m.contains("context window")
        || m.contains("too many tokens")
        || m.contains("reduce the length")
        || m.contains("input is too long")
        || (m.contains("token") && m.contains("exceed"))
}

/// Best-effort extraction of an explicit context-window size (in tokens) from a
/// raw provider error body, e.g. OpenAI's "maximum context length is 8192 tokens".
/// Returns the first integer following a known marker phrase.
pub fn parse_context_limit(body: &str) -> Option<usize> {
    let lower = body.to_lowercase();
    const MARKERS: &[&str] = &[
        "maximum context length is",
        "maximum context length of",
        "context length is",
        "context window of",
        "context window is",
    ];
    for marker in MARKERS {
        if let Some(pos) = lower.find(marker) {
            if let Some(n) = first_integer(&lower[pos + marker.len()..]) {
                return Some(n);
            }
        }
    }
    None
}

fn first_integer(s: &str) -> Option<usize> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_overflow_phrasings() {
        assert!(is_context_overflow("⚠️ **Prompt Too Large**"));
        assert!(is_context_overflow(
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(is_context_overflow("Input is too long for the context window"));
        assert!(!is_context_overflow("Authentication failed"));
    }

    #[test]
    fn parses_explicit_limit() {
        assert_eq!(
            parse_context_limit(
                "This model's maximum context length is 8192 tokens. However, your messages resulted in 10000 tokens."
            ),
            Some(8192)
        );
        assert_eq!(parse_context_limit("context window of 200000"), Some(200000));
        assert_eq!(parse_context_limit("some unrelated error"), None);
    }

    #[test]
    fn records_and_clamps_to_smallest() {
        // Use a unique scope so tests don't collide in the shared static.
        let scope = "test://record-clamp";
        let model = "modelA";
        assert_eq!(learned_window(scope, model), None);

        record_window(scope, model, 32_000);
        assert_eq!(learned_window(scope, model), Some(32_000));

        // A larger later value must not raise the learned ceiling.
        record_window(scope, model, 64_000);
        assert_eq!(learned_window(scope, model), Some(32_000));

        // A smaller value lowers it.
        record_window(scope, model, 16_000);
        assert_eq!(learned_window(scope, model), Some(16_000));
    }

    #[test]
    fn clamp_takes_minimum_and_fills_none() {
        let scope = "test://clamp-min";
        let model = "modelB";
        // No learned value: base passes through.
        assert_eq!(clamp_to_learned(scope, model, Some(500_000)), Some(500_000));
        assert_eq!(clamp_to_learned(scope, model, None), None);

        record_window(scope, model, 128_000);
        assert_eq!(clamp_to_learned(scope, model, Some(500_000)), Some(128_000));
        // base None but learned present → learned becomes the window.
        assert_eq!(clamp_to_learned(scope, model, None), Some(128_000));
        // base smaller than learned stays.
        assert_eq!(clamp_to_learned(scope, model, Some(64_000)), Some(64_000));
    }

    #[test]
    fn zero_is_ignored() {
        let scope = "test://zero";
        record_window(scope, "m", 0);
        assert_eq!(learned_window(scope, "m"), None);
    }
}
