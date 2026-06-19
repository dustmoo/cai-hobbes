//! Best-effort context-window lookup for OpenAI and common OpenAI-compatible
//! hosted models.
//!
//! The OpenAI `/v1/models` endpoint deliberately omits context-window size, and
//! there is no reliable cross-vendor way to query it at runtime (see the
//! adapt-to-error path in `stream_manager` and the learned cache in
//! `context_cache`). This table is a heuristic used for *proactive* budgeting so
//! a configured-but-unspecified endpoint still gets a sensible window instead of
//! `None`. Any mismatch is corrected at runtime when the server rejects an
//! oversized prompt.
//!
//! Source: OpenAI model documentation (2026). Windows change over time; treat
//! this as a starting estimate, not a contract.

/// Resolve a known context window (in tokens) for an OpenAI-style model name,
/// matched by lowercased prefix. Returns `None` for unknown/custom models so the
/// caller can fall back to a user override or the learned-window cache.
///
/// Prefix order matters: more specific names (`gpt-5.4`, `gpt-5.5`) must precede
/// the broader `gpt-5` so they aren't shadowed.
pub fn known_context_window(model: &str) -> Option<usize> {
    let m = model.trim().to_ascii_lowercase();

    // (prefix, window_tokens) — checked top to bottom, first match wins.
    const TABLE: &[(&str, usize)] = &[
        // GPT-4.1 family: 1M context.
        ("gpt-4.1", 1_000_000),
        // GPT-5.x: specific minor versions before the broad `gpt-5`.
        ("gpt-5.4", 1_000_000),
        ("gpt-5.5", 512_000),
        ("gpt-5", 400_000),
        // GPT-4o / 4-turbo: 128K.
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        // Reasoning o-series: 200K.
        ("o4", 200_000),
        ("o3", 200_000),
        ("o1", 200_000),
    ];

    TABLE
        .iter()
        .find(|(prefix, _)| m.starts_with(prefix))
        .map(|(_, window)| *window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_families() {
        assert_eq!(known_context_window("gpt-4.1"), Some(1_000_000));
        assert_eq!(known_context_window("gpt-4.1-mini"), Some(1_000_000));
        assert_eq!(known_context_window("gpt-5.4"), Some(1_000_000));
        assert_eq!(known_context_window("gpt-5.5"), Some(512_000));
        // Bare gpt-5 / gpt-5-mini must NOT be shadowed by the 5.4/5.5 entries.
        assert_eq!(known_context_window("gpt-5"), Some(400_000));
        assert_eq!(known_context_window("gpt-5-mini"), Some(400_000));
        assert_eq!(known_context_window("gpt-4o"), Some(128_000));
        assert_eq!(known_context_window("o3-mini"), Some(200_000));
    }

    #[test]
    fn case_insensitive_and_trimmed() {
        assert_eq!(known_context_window("  GPT-4.1-Mini "), Some(1_000_000));
    }

    #[test]
    fn unknown_models_return_none() {
        assert_eq!(known_context_window("llama3.2"), None);
        assert_eq!(known_context_window("qwen3-32b"), None);
        assert_eq!(known_context_window(""), None);
    }
}
