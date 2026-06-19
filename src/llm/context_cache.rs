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

#[cfg(test)]
mod tests {
    use super::*;

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
