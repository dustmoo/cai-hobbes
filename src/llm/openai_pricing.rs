//! OpenAI API cost estimation.
//!
//! Costs are only meaningful for the **real OpenAI API** (`api.openai.com` with
//! an API key). Local / self-hosted OpenAI-compatible servers (Ollama, vLLM,
//! LM Studio) and proxies are free or have unknowable pricing, so we never
//! fabricate a cost for them — see [`billable`].
//!
//! Prices are USD per 1M tokens. This is a **manually maintained** table that
//! drifts as OpenAI revises pricing — re-verify against the official page and
//! bump the date when you touch it (see SYSTEM_PATTERNS P-013).
//! Last verified: 2026-09-17. Added the new gpt-6 flagship tier (`gpt-6-astra`)
//! and the gpt-5.6 family (`sol`/`terra`/`luna`); confirmed gpt-5.5 unchanged.
//! Reasoning tokens are billed as output tokens and are already included in the
//! reported output count, so no special handling is needed. Cached-input
//! discounts are recorded for reference but not yet applied (our stream parsers
//! don't surface the cached-token split).

/// Per-1M-token pricing for an OpenAI model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenAiPrice {
    pub input_per_1m: f64,
    #[allow(dead_code)]
    pub cached_input_per_1m: f64,
    pub output_per_1m: f64,
}

/// Pricing table, ordered **most-specific prefix first** so that e.g. `gpt-5.5`
/// and `gpt-5-mini` are matched before the bare `gpt-5` entry. Matching is by
/// `starts_with`, which also covers dated snapshots (`gpt-5-2026-…`).
const PRICES: &[(&str, OpenAiPrice)] = &[
    // ── gpt-6 family ──────────────────────────────────────────────────
    ("gpt-6-astra", OpenAiPrice { input_per_1m: 10.0, cached_input_per_1m: 1.0, output_per_1m: 50.0 }),
    // ── gpt-5.6 family ────────────────────────────────────────────────
    // Sol's standard rate is $5/$0.50/$30 (same as gpt-5.5); OpenAI is running a
    // promotional rate of $4/$0.40/$20 through at least 2026-11-21. We record the
    // current (promotional) rate here — re-check after that date and revert to
    // standard if the promo has ended.
    ("gpt-5.6-sol", OpenAiPrice { input_per_1m: 4.0, cached_input_per_1m: 0.40, output_per_1m: 20.0 }),
    ("gpt-5.6-terra", OpenAiPrice { input_per_1m: 2.0, cached_input_per_1m: 0.20, output_per_1m: 12.0 }),
    ("gpt-5.6-luna", OpenAiPrice { input_per_1m: 0.20, cached_input_per_1m: 0.02, output_per_1m: 1.20 }),
    // ── gpt-5.5 family ────────────────────────────────────────────────
    ("gpt-5.5-pro", OpenAiPrice { input_per_1m: 30.0, cached_input_per_1m: 30.0, output_per_1m: 180.0 }),
    ("gpt-5.5", OpenAiPrice { input_per_1m: 5.0, cached_input_per_1m: 0.50, output_per_1m: 30.0 }),
    // ── gpt-5.4 family ────────────────────────────────────────────────
    ("gpt-5.4-pro", OpenAiPrice { input_per_1m: 30.0, cached_input_per_1m: 30.0, output_per_1m: 180.0 }),
    ("gpt-5.4-mini", OpenAiPrice { input_per_1m: 0.75, cached_input_per_1m: 0.075, output_per_1m: 4.50 }),
    ("gpt-5.4-nano", OpenAiPrice { input_per_1m: 0.20, cached_input_per_1m: 0.02, output_per_1m: 1.25 }),
    ("gpt-5.4", OpenAiPrice { input_per_1m: 2.50, cached_input_per_1m: 0.25, output_per_1m: 15.0 }),
    // ── gpt-5 family ──────────────────────────────────────────────────
    ("gpt-5-pro", OpenAiPrice { input_per_1m: 15.0, cached_input_per_1m: 15.0, output_per_1m: 120.0 }),
    ("gpt-5-mini", OpenAiPrice { input_per_1m: 0.25, cached_input_per_1m: 0.025, output_per_1m: 2.0 }),
    ("gpt-5-nano", OpenAiPrice { input_per_1m: 0.05, cached_input_per_1m: 0.005, output_per_1m: 0.40 }),
    ("gpt-5", OpenAiPrice { input_per_1m: 1.25, cached_input_per_1m: 0.125, output_per_1m: 10.0 }),
    // ── gpt-4.1 family ────────────────────────────────────────────────
    ("gpt-4.1-nano", OpenAiPrice { input_per_1m: 0.10, cached_input_per_1m: 0.025, output_per_1m: 0.40 }),
    ("gpt-4.1-mini", OpenAiPrice { input_per_1m: 0.40, cached_input_per_1m: 0.10, output_per_1m: 1.60 }),
    ("gpt-4.1", OpenAiPrice { input_per_1m: 2.0, cached_input_per_1m: 0.50, output_per_1m: 8.0 }),
    // ── gpt-4o family ─────────────────────────────────────────────────
    ("gpt-4o-mini", OpenAiPrice { input_per_1m: 0.15, cached_input_per_1m: 0.075, output_per_1m: 0.60 }),
    ("gpt-4o", OpenAiPrice { input_per_1m: 2.50, cached_input_per_1m: 1.25, output_per_1m: 10.0 }),
    // ── o-series reasoning models ─────────────────────────────────────
    ("o4-mini", OpenAiPrice { input_per_1m: 1.10, cached_input_per_1m: 0.275, output_per_1m: 4.40 }),
    ("o3-mini", OpenAiPrice { input_per_1m: 1.10, cached_input_per_1m: 0.55, output_per_1m: 4.40 }),
    ("o3", OpenAiPrice { input_per_1m: 2.0, cached_input_per_1m: 0.50, output_per_1m: 8.0 }),
    ("o1-mini", OpenAiPrice { input_per_1m: 1.10, cached_input_per_1m: 0.55, output_per_1m: 4.40 }),
    ("o1-pro", OpenAiPrice { input_per_1m: 150.0, cached_input_per_1m: 150.0, output_per_1m: 600.0 }),
    ("o1", OpenAiPrice { input_per_1m: 15.0, cached_input_per_1m: 7.50, output_per_1m: 60.0 }),
];

/// Whether `model` belongs to the family named by `prefix`. Matches the exact
/// slug or a `-`-delimited suffix (a dated snapshot, e.g. `gpt-5-2026-01-15`),
/// but NOT a different minor version: `gpt-5.1` must not match `gpt-5`. A bare
/// `starts_with` would silently price `gpt-5.1` / `gpt-5.2` at gpt-5's rate.
fn matches_family(model: &str, prefix: &str) -> bool {
    match model.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('-'),
        None => false,
    }
}

/// Look up per-1M pricing for a model slug. Returns `None` for models not in the
/// table (including unrecognized version families) so callers can leave the cost
/// unset rather than display a fabricated or wrong price.
pub fn price_for_model(model: &str) -> Option<OpenAiPrice> {
    let m = model.trim();
    PRICES
        .iter()
        .find(|(prefix, _)| matches_family(m, prefix))
        .map(|(_, price)| *price)
}

/// Whether usage on this endpoint should be billed. Only the real OpenAI API
/// (with a key) has a known, chargeable price; local servers and keyless
/// endpoints are treated as free.
pub fn billable(endpoint: &str, has_api_key: bool) -> bool {
    has_api_key && endpoint.contains("api.openai.com")
}

/// Estimate the USD cost of a turn from input/output token counts.
/// Returns `None` for unknown models (cost intentionally left unset).
pub fn estimate_cost(model: &str, input_tokens: i64, output_tokens: i64) -> Option<f64> {
    let price = price_for_model(model)?;
    let input = (input_tokens.max(0) as f64 / 1_000_000.0) * price.input_per_1m;
    let output = (output_tokens.max(0) as f64 / 1_000_000.0) * price.output_per_1m;
    Some(input + output)
}

/// Resolve the cost to record for a turn:
/// - real OpenAI + key → estimated cost (`None` if the model is unknown)
/// - local / keyless    → `Some(0.0)` (free)
pub fn turn_cost(
    endpoint: &str,
    has_api_key: bool,
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    if billable(endpoint, has_api_key) {
        estimate_cost(model, input_tokens, output_tokens)
    } else {
        Some(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(model: &str) -> OpenAiPrice {
        price_for_model(model).unwrap_or_else(|| panic!("no price for {model}"))
    }

    #[test]
    fn specific_prefixes_win_over_general_ones() {
        // The whole point of the ordering: variants must not collapse to the
        // bare family price.
        assert_eq!(rate("gpt-5").output_per_1m, 10.0);
        assert_eq!(rate("gpt-5-mini").output_per_1m, 2.0);
        assert_eq!(rate("gpt-5-nano").output_per_1m, 0.40);
        assert_eq!(rate("gpt-5-pro").output_per_1m, 120.0);
        assert_eq!(rate("gpt-5.5").output_per_1m, 30.0);
        assert_eq!(rate("gpt-5.5-pro").output_per_1m, 180.0);
        assert_eq!(rate("gpt-5.4").output_per_1m, 15.0);
        assert_eq!(rate("gpt-5.4-mini").output_per_1m, 4.50);
        assert_eq!(rate("gpt-4o").input_per_1m, 2.50);
        assert_eq!(rate("gpt-4o-mini").input_per_1m, 0.15);
        assert_eq!(rate("o3").output_per_1m, 8.0);
        assert_eq!(rate("o3-mini").output_per_1m, 4.40);
        assert_eq!(rate("o4-mini").output_per_1m, 4.40);
    }

    #[test]
    fn gpt6_and_gpt5_6_family_prices_resolve_correctly() {
        assert_eq!(rate("gpt-6-astra").input_per_1m, 10.0);
        assert_eq!(rate("gpt-6-astra").cached_input_per_1m, 1.0);
        assert_eq!(rate("gpt-6-astra").output_per_1m, 50.0);

        assert_eq!(rate("gpt-5.6-sol").input_per_1m, 4.0);
        assert_eq!(rate("gpt-5.6-sol").cached_input_per_1m, 0.40);
        assert_eq!(rate("gpt-5.6-sol").output_per_1m, 20.0);

        assert_eq!(rate("gpt-5.6-terra").input_per_1m, 2.0);
        assert_eq!(rate("gpt-5.6-terra").output_per_1m, 12.0);

        assert_eq!(rate("gpt-5.6-luna").input_per_1m, 0.20);
        assert_eq!(rate("gpt-5.6-luna").output_per_1m, 1.20);

        // gpt-5.5 is unchanged by the new tiers above it.
        assert_eq!(rate("gpt-5.5").input_per_1m, 5.0);
        assert_eq!(rate("gpt-5.5").output_per_1m, 30.0);
    }

    #[test]
    fn gpt5_6_and_gpt6_do_not_fall_through_to_gpt5_or_gpt5_5() {
        // gpt-5.6-* must not collapse to the bare gpt-5 rate ($1.25/$10) nor to
        // gpt-5.5's rate ($5/$30) — each has its own confirmed entry.
        let gpt5 = price_for_model("gpt-5").unwrap();
        let gpt5_5 = price_for_model("gpt-5.5").unwrap();
        for v in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let p = price_for_model(v).unwrap();
            assert_ne!(p, gpt5, "{v} must not inherit bare gpt-5 pricing");
            assert_ne!(p, gpt5_5, "{v} must not inherit gpt-5.5 pricing");
        }
        // gpt-6-astra is a distinct family with no colliding prefix.
        let astra = price_for_model("gpt-6-astra").unwrap();
        assert_ne!(astra, gpt5);
        assert_ne!(astra, gpt5_5);
    }

    #[test]
    fn gpt6_and_gpt5_6_dated_snapshots_match_their_family() {
        assert_eq!(price_for_model("gpt-6-astra-2026-09-03"), price_for_model("gpt-6-astra"));
        assert_eq!(price_for_model("gpt-5.6-sol-2026-09-03"), price_for_model("gpt-5.6-sol"));
    }

    #[test]
    fn dated_snapshots_match_their_family() {
        assert_eq!(price_for_model("gpt-5-2026-01-15"), price_for_model("gpt-5"));
        assert_eq!(price_for_model("gpt-5-mini-2026-01-15"), price_for_model("gpt-5-mini"));
        assert_eq!(price_for_model("gpt-5.5-2026-03-01"), price_for_model("gpt-5.5"));
    }

    #[test]
    fn unknown_models_have_no_price() {
        assert!(price_for_model("llama-3.3-70b").is_none());
        assert!(price_for_model("qwen2.5-coder").is_none());
        assert!(estimate_cost("some-local-model", 1000, 1000).is_none());
    }

    #[test]
    fn unrecognized_minor_versions_do_not_inherit_bare_gpt5_price() {
        // The bug this guards: `starts_with("gpt-5")` would price gpt-5.1/5.2/5.3
        // at gpt-5's $1.25/$10. Without explicit entries they must resolve to
        // None (no fabricated/too-cheap cost), never to bare gpt-5.
        let gpt5 = price_for_model("gpt-5").unwrap();
        for v in ["gpt-5.1", "gpt-5.2", "gpt-5.3", "gpt-5.1-mini", "gpt-5.9"] {
            let p = price_for_model(v);
            assert!(p.is_none() || p != Some(gpt5), "{v} must not inherit bare gpt-5 pricing");
            assert!(p.is_none(), "{v} has no confirmed price → expected None, got {:?}", p);
        }
        // Sanity: the families we DO know still resolve correctly across the dot.
        assert_eq!(price_for_model("gpt-5.4").unwrap().output_per_1m, 15.0);
        assert_eq!(price_for_model("gpt-5.5").unwrap().output_per_1m, 30.0);
    }

    #[test]
    fn estimate_cost_math() {
        // gpt-5: $1.25 input + $10 output per 1M. 1M in + 1M out = 11.25.
        let c = estimate_cost("gpt-5", 1_000_000, 1_000_000).unwrap();
        assert!((c - 11.25).abs() < 1e-9, "got {c}");
        // Half-million output only on gpt-5-mini ($2/1M) = $1.00.
        let c = estimate_cost("gpt-5-mini", 0, 500_000).unwrap();
        assert!((c - 1.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn billable_only_for_real_openai_with_key() {
        assert!(billable("https://api.openai.com/v1", true));
        assert!(!billable("https://api.openai.com/v1", false)); // no key
        assert!(!billable("http://localhost:11434/v1", true)); // local
        assert!(!billable("https://openrouter.ai/api/v1", true)); // proxy
    }

    #[test]
    fn turn_cost_gates_by_endpoint() {
        // Real OpenAI, known model → estimated.
        let c = turn_cost("https://api.openai.com/v1", true, "gpt-5", 1_000_000, 0).unwrap();
        assert!((c - 1.25).abs() < 1e-9, "got {c}");
        // Local → free.
        assert_eq!(turn_cost("http://localhost:11434/v1", false, "llama3", 1_000_000, 999), Some(0.0));
        // Real OpenAI but unknown model → no fabricated cost.
        assert_eq!(turn_cost("https://api.openai.com/v1", true, "mystery-model", 10, 10), None);
    }
}
