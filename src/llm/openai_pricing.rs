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
//! Last verified: 2026-11-05 against developers.openai.com/api/docs/pricing.
//! Added gpt-6.1 (`sol`/`terra`), gpt-6-sol, gpt-6-luna and gpt-6-rosalind;
//! confirmed gpt-6-astra ($10/$1/$50) and the gpt-5.6 / gpt-5.5 rows unchanged.
//! Reasoning tokens are billed as output tokens and are already included in the
//! reported output count, so no special handling is needed.
//!
//! Cached input: OpenAI reports cached tokens as a subset of `input_tokens`
//! (`input_tokens_details.cached_tokens` / `prompt_tokens_details.cached_tokens`);
//! those are billed at the cached rate, the rest at the full input rate.
//!
//! Long context: on the gpt-5.5 and later families, a request with more than
//! [`LONG_CONTEXT_THRESHOLD`] input tokens is billed at 2x input / cached input
//! and 1.5x output for the whole request.
//!
//! Not modelled: cache-write surcharges (the API doesn't report write counts),
//! Batch / Flex / Fast tiers, and regional-processing uplift.

/// Per-1M-token pricing for an OpenAI model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpenAiPrice {
    pub input_per_1m: f64,
    pub cached_input_per_1m: f64,
    pub output_per_1m: f64,
    /// Whether requests over [`LONG_CONTEXT_THRESHOLD`] input tokens are billed
    /// at the long-context rate (2x input and cached input, 1.5x output).
    pub long_context: bool,
}

/// Input-token count above which long-context pricing applies (">272K").
pub const LONG_CONTEXT_THRESHOLD: i64 = 272_000;

const fn p(input: f64, cached: f64, output: f64) -> OpenAiPrice {
    OpenAiPrice { input_per_1m: input, cached_input_per_1m: cached, output_per_1m: output, long_context: false }
}

/// A price with the long-context surcharge (gpt-5.5 and later).
const fn lc(input: f64, cached: f64, output: f64) -> OpenAiPrice {
    OpenAiPrice { input_per_1m: input, cached_input_per_1m: cached, output_per_1m: output, long_context: true }
}

/// Pricing table, ordered **most-specific prefix first** so that e.g. `gpt-5.5`
/// and `gpt-5-mini` are matched before the bare `gpt-5` entry. Matching is by
/// `starts_with`, which also covers dated snapshots (`gpt-5-2026-…`).
const PRICES: &[(&str, OpenAiPrice)] = &[
    // ── gpt-6.1 family ────────────────────────────────────────────────
    ("gpt-6.1-sol", lc(2.0, 0.10, 10.0)),
    ("gpt-6.1-terra", lc(1.0, 0.05, 5.0)),
    // ── gpt-6 family ──────────────────────────────────────────────────
    // `gpt-6-astra-law` matches `gpt-6-astra` (same $10/$1/$50).
    ("gpt-6-astra", lc(10.0, 1.0, 50.0)),
    ("gpt-6-rosalind", lc(15.0, 1.50, 75.0)),
    ("gpt-6-sol", lc(2.0, 0.20, 10.0)),
    ("gpt-6-luna", lc(0.10, 0.01, 0.50)),
    ("gpt-6-cyber", p(20.0, 2.0, 100.0)),
    // ── gpt-5.6 family ────────────────────────────────────────────────
    // OpenAI notes a gpt-5.6-sol promotional rate until 2026-11-30 without
    // listing it; $4/$0.40/$20 is the rate on the Standard table.
    ("gpt-5.6-sol", lc(4.0, 0.40, 20.0)),
    ("gpt-5.6-terra", lc(2.0, 0.20, 12.0)),
    ("gpt-5.6-luna", lc(0.20, 0.02, 1.20)),
    ("gpt-5.6-cyber", p(12.50, 1.25, 75.0)),
    // ── gpt-5.5 family ────────────────────────────────────────────────
    ("gpt-5.5-pro", lc(30.0, 30.0, 180.0)),
    ("gpt-5.5-cyber", p(12.50, 1.25, 75.0)),
    ("gpt-5.5", lc(5.0, 0.50, 30.0)),
    // ── gpt-5.4 family ────────────────────────────────────────────────
    ("gpt-5.4-pro", p(30.0, 30.0, 180.0)),
    ("gpt-5.4-mini", p(0.75, 0.075, 4.50)),
    ("gpt-5.4-nano", p(0.20, 0.02, 1.25)),
    ("gpt-5.4", p(2.50, 0.25, 15.0)),
    // ── gpt-5 family ──────────────────────────────────────────────────
    ("gpt-5-pro", p(15.0, 15.0, 120.0)),
    ("gpt-5-mini", p(0.25, 0.025, 2.0)),
    ("gpt-5-nano", p(0.05, 0.005, 0.40)),
    ("gpt-5", p(1.25, 0.125, 10.0)),
    // ── gpt-4.1 family ────────────────────────────────────────────────
    ("gpt-4.1-nano", p(0.10, 0.025, 0.40)),
    ("gpt-4.1-mini", p(0.40, 0.10, 1.60)),
    ("gpt-4.1", p(2.0, 0.50, 8.0)),
    // ── gpt-4o family ─────────────────────────────────────────────────
    ("gpt-4o-mini", p(0.15, 0.075, 0.60)),
    ("gpt-4o", p(2.50, 1.25, 10.0)),
    // ── o-series reasoning models ─────────────────────────────────────
    ("o4-mini", p(1.10, 0.275, 4.40)),
    ("o3-mini", p(1.10, 0.55, 4.40)),
    ("o3", p(2.0, 0.50, 8.0)),
    ("o1-mini", p(1.10, 0.55, 4.40)),
    ("o1-pro", p(150.0, 150.0, 600.0)),
    ("o1", p(15.0, 7.50, 60.0)),
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

/// Estimate the USD cost of a turn. `cached_tokens` is the part of
/// `input_tokens` served from OpenAI's prompt cache (a subset, not extra), billed
/// at the cached rate. Returns `None` for unknown models (cost left unset).
pub fn estimate_cost(
    model: &str,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    let price = price_for_model(model)?;
    let input = input_tokens.max(0);
    let cached = cached_tokens.clamp(0, input);
    let (in_mul, out_mul) = if price.long_context && input > LONG_CONTEXT_THRESHOLD {
        (2.0, 1.5)
    } else {
        (1.0, 1.0)
    };
    let per_m = |tokens: i64, rate: f64| (tokens as f64 / 1_000_000.0) * rate;
    let input_cost = per_m(input - cached, price.input_per_1m) + per_m(cached, price.cached_input_per_1m);
    let output_cost = per_m(output_tokens.max(0), price.output_per_1m);
    Some(input_cost * in_mul + output_cost * out_mul)
}

/// Resolve the cost to record for a turn:
/// - real OpenAI + key → estimated cost (`None` if the model is unknown)
/// - local / keyless    → `Some(0.0)` (free)
pub fn turn_cost(
    endpoint: &str,
    has_api_key: bool,
    model: &str,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
) -> Option<f64> {
    if billable(endpoint, has_api_key) {
        estimate_cost(model, input_tokens, cached_tokens, output_tokens)
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
        assert!(estimate_cost("some-local-model", 1000, 0, 1000).is_none());
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
        let c = estimate_cost("gpt-5", 1_000_000, 0, 1_000_000).unwrap();
        assert!((c - 11.25).abs() < 1e-9, "got {c}");
        // Half-million output only on gpt-5-mini ($2/1M) = $1.00.
        let c = estimate_cost("gpt-5-mini", 0, 0, 500_000).unwrap();
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
        let c = turn_cost("https://api.openai.com/v1", true, "gpt-5", 1_000_000, 0, 0).unwrap();
        assert!((c - 1.25).abs() < 1e-9, "got {c}");
        // Local → free.
        assert_eq!(turn_cost("http://localhost:11434/v1", false, "llama3", 1_000_000, 0, 999), Some(0.0));
        // Real OpenAI but unknown model → no fabricated cost.
        assert_eq!(turn_cost("https://api.openai.com/v1", true, "mystery-model", 10, 0, 10), None);
    }

    #[test]
    fn gpt6_1_and_new_gpt6_tiers_resolve() {
        assert_eq!(rate("gpt-6.1-terra"), lc(1.0, 0.05, 5.0));
        assert_eq!(rate("gpt-6.1-sol"), lc(2.0, 0.10, 10.0));
        assert_eq!(rate("gpt-6-sol"), lc(2.0, 0.20, 10.0));
        assert_eq!(rate("gpt-6-luna"), lc(0.10, 0.01, 0.50));
        assert_eq!(rate("gpt-6-rosalind").output_per_1m, 75.0);
        // gpt-6.1-* must not fall through to a gpt-6 row, nor gpt-6-sol to 6.1.
        assert_ne!(rate("gpt-6.1-sol"), rate("gpt-6-sol"));
        assert_eq!(price_for_model("gpt-6.1-terra-2026-10-26"), price_for_model("gpt-6.1-terra"));
        assert_eq!(price_for_model("gpt-6-astra-law"), price_for_model("gpt-6-astra"));
        assert!(price_for_model("gpt-6.2-sol").is_none());
    }

    #[test]
    fn cached_input_is_billed_at_the_cached_rate() {
        // gpt-6-astra: 100K input of which 90K cached, 1K output.
        // 10K * $10 + 90K * $1 + 1K * $50 = $0.10 + $0.09 + $0.05 = $0.24.
        let c = estimate_cost("gpt-6-astra", 100_000, 90_000, 1_000).unwrap();
        assert!((c - 0.24).abs() < 1e-9, "got {c}");
        // Without the split the same turn would have cost $1.05.
        let full = estimate_cost("gpt-6-astra", 100_000, 0, 1_000).unwrap();
        assert!((full - 1.05).abs() < 1e-9, "got {full}");
        // A cached count larger than the input is clamped, never negative.
        let c = estimate_cost("gpt-6-astra", 1_000, 5_000, 0).unwrap();
        assert!((c - 0.001).abs() < 1e-12, "got {c}");
    }

    #[test]
    fn long_context_surcharge_applies_over_272k_only_where_listed() {
        // gpt-6-astra at 300K uncached input + 10K output:
        // 300K * $20 + 10K * $75 = $6.00 + $0.75 = $6.75.
        let c = estimate_cost("gpt-6-astra", 300_000, 0, 10_000).unwrap();
        assert!((c - 6.75).abs() < 1e-9, "got {c}");
        // At exactly the threshold it's still short-context: 272K * $10 = $2.72.
        let c = estimate_cost("gpt-6-astra", 272_000, 0, 0).unwrap();
        assert!((c - 2.72).abs() < 1e-9, "got {c}");
        // gpt-4.1 has no long-context tier.
        let c = estimate_cost("gpt-4.1", 300_000, 0, 0).unwrap();
        assert!((c - 0.60).abs() < 1e-9, "got {c}");
    }
}
