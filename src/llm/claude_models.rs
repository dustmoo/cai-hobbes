//! Claude (Anthropic) model metadata — the single source of truth for model
//! identifiers, context windows, output caps, and pricing used by the Claude
//! connector, the context-budget tuning, and the model-slot picker UI.
//!
//! Mirrors the structure of [`super::gemini::GeminiModel`].
//!
//! Model IDs are the canonical Anthropic **aliases**. Do NOT append date
//! suffixes to aliases (e.g. use `claude-opus-4-8`, never
//! `claude-opus-4-8-20260…`). Full dated IDs that the API may echo back are
//! still recognized by [`ClaudeModel::from_slug`] via prefix matching.
//!
//! Source: Anthropic model catalog (Messages API). Capability notes:
//! - Modern models (Fable 5.1, the Claude 5 family, Opus 4.6+, Sonnet 4.6) use
//!   **adaptive** thinking (`thinking: {type: "adaptive"}`); `budget_tokens`
//!   is removed and returns 400. All Claude models support image input
//!   (vision).
//! - Opus 5.5 and Sonnet 5.5 (September 2026) are the current flagship
//!   generation; Fable 5.1 sits above them as the Mythos-class tier. Opus 5,
//!   Sonnet 5, Fable 5 and the 4.x models are legacy but still live.
//! - Fable 5.1 and Opus 5.5 think on every request: `thinking: disabled`
//!   returns 400, and thinking text is omitted unless `display` asks for it
//!   (see [`ClaudeModel::thinking_always_on`]).
//! - Opus 4.1 is retired on the Claude API (Bedrock/Google Cloud only), so it
//!   is kept for parsing and pricing old sessions but left out of the picker.

/// Supported Claude models for metadata, pricing, and UI display.
#[derive(Debug, PartialEq, Clone)]
pub enum ClaudeModel {
    /// Mythos-class, above Opus 5 — the most capable model in the family.
    Fable5_1,
    /// Retained: the pre-5.1 Fable alias. Distinct pricing/caps from Fable5_1
    /// (see `SYSTEM_PATTERNS` P-013 verification note below).
    Fable5,
    /// Current flagship (September 2026).
    Opus5_5,
    /// Legacy: the previous flagship.
    Opus5,
    Opus4_8,
    Opus4_7,
    Opus4_6,
    Opus4_5,
    Opus4_1,
    /// Current mid-tier (September 2026).
    Sonnet5_5,
    /// Legacy: the previous mid-tier.
    Sonnet5,
    Sonnet4_6,
    Sonnet4_5,
    /// Current small model (October 2026). Priced by prompt length: higher
    /// rates above 100k prompt tokens (see [`ClaudeModel::price_multiplier`]).
    Haiku5_5,
    Haiku4_5,
    /// Any unrecognized slug — handled gracefully (conservative defaults).
    Unknown(String),
}

impl ClaudeModel {
    /// The default chat model shipped with Hobbes.
    pub const DEFAULT_CHAT_SLUG: &'static str = "claude-opus-5-5";

    /// Parse a model slug (alias or full dated ID) into a [`ClaudeModel`].
    pub fn from_slug(slug: &str) -> Self {
        let s = slug.trim();
        match s {
            _ if s.starts_with("claude-fable-5-1") => ClaudeModel::Fable5_1,
            _ if s.starts_with("claude-fable-5") => ClaudeModel::Fable5,
            // Point releases before their bare prefix: "claude-opus-5-5"
            // also starts with "claude-opus-5".
            _ if s.starts_with("claude-opus-5-5") => ClaudeModel::Opus5_5,
            _ if s.starts_with("claude-opus-5") => ClaudeModel::Opus5,
            _ if s.starts_with("claude-opus-4-8") => ClaudeModel::Opus4_8,
            _ if s.starts_with("claude-opus-4-7") => ClaudeModel::Opus4_7,
            _ if s.starts_with("claude-opus-4-6") => ClaudeModel::Opus4_6,
            _ if s.starts_with("claude-opus-4-5") => ClaudeModel::Opus4_5,
            _ if s.starts_with("claude-opus-4-1") => ClaudeModel::Opus4_1,
            _ if s.starts_with("claude-sonnet-5-5") => ClaudeModel::Sonnet5_5,
            _ if s.starts_with("claude-sonnet-5") => ClaudeModel::Sonnet5,
            _ if s.starts_with("claude-sonnet-4-6") => ClaudeModel::Sonnet4_6,
            _ if s.starts_with("claude-sonnet-4-5") => ClaudeModel::Sonnet4_5,
            _ if s.starts_with("claude-haiku-5-5") => ClaudeModel::Haiku5_5,
            _ if s.starts_with("claude-haiku-4-5") => ClaudeModel::Haiku4_5,
            other => ClaudeModel::Unknown(other.to_string()),
        }
    }

    /// The canonical alias to send to the API.
    pub fn canonical_slug(&self) -> &str {
        match self {
            ClaudeModel::Fable5_1 => "claude-fable-5-1",
            ClaudeModel::Fable5 => "claude-fable-5",
            ClaudeModel::Opus5_5 => "claude-opus-5-5",
            ClaudeModel::Opus5 => "claude-opus-5",
            ClaudeModel::Opus4_8 => "claude-opus-4-8",
            ClaudeModel::Opus4_7 => "claude-opus-4-7",
            ClaudeModel::Opus4_6 => "claude-opus-4-6",
            ClaudeModel::Opus4_5 => "claude-opus-4-5",
            ClaudeModel::Opus4_1 => "claude-opus-4-1",
            ClaudeModel::Sonnet5_5 => "claude-sonnet-5-5",
            ClaudeModel::Sonnet5 => "claude-sonnet-5",
            ClaudeModel::Sonnet4_6 => "claude-sonnet-4-6",
            ClaudeModel::Sonnet4_5 => "claude-sonnet-4-5",
            ClaudeModel::Haiku5_5 => "claude-haiku-5-5",
            ClaudeModel::Haiku4_5 => "claude-haiku-4-5",
            ClaudeModel::Unknown(slug) => slug,
        }
    }

    /// Human-readable name for the UI.
    pub fn display_name(&self) -> String {
        match self {
            ClaudeModel::Fable5_1 => "Claude Fable 5.1".to_string(),
            ClaudeModel::Fable5 => "Claude Fable 5".to_string(),
            ClaudeModel::Opus5_5 => "Claude Opus 5.5".to_string(),
            ClaudeModel::Opus5 => "Claude Opus 5".to_string(),
            ClaudeModel::Opus4_8 => "Claude Opus 4.8".to_string(),
            ClaudeModel::Opus4_7 => "Claude Opus 4.7".to_string(),
            ClaudeModel::Opus4_6 => "Claude Opus 4.6".to_string(),
            ClaudeModel::Opus4_5 => "Claude Opus 4.5".to_string(),
            ClaudeModel::Opus4_1 => "Claude Opus 4.1".to_string(),
            ClaudeModel::Sonnet5_5 => "Claude Sonnet 5.5".to_string(),
            ClaudeModel::Sonnet5 => "Claude Sonnet 5".to_string(),
            ClaudeModel::Sonnet4_6 => "Claude Sonnet 4.6".to_string(),
            ClaudeModel::Sonnet4_5 => "Claude Sonnet 4.5".to_string(),
            ClaudeModel::Haiku5_5 => "Claude Haiku 5.5".to_string(),
            ClaudeModel::Haiku4_5 => "Claude Haiku 4.5".to_string(),
            ClaudeModel::Unknown(slug) => slug
                .replace(['-', '_'], " ")
                .split_whitespace()
                .map(|word| {
                    let mut c = word.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<String>>()
                .join(" "),
        }
    }

    /// Maximum input context window in tokens.
    /// Reserved for context-budget tuning (parity with `GeminiModel`).
    #[allow(dead_code)]
    pub fn context_window_tokens(&self) -> usize {
        match self {
            // 1M context window at standard pricing.
            ClaudeModel::Fable5_1
            | ClaudeModel::Fable5
            | ClaudeModel::Opus5_5
            | ClaudeModel::Opus5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6
            | ClaudeModel::Sonnet5_5
            | ClaudeModel::Sonnet5
            | ClaudeModel::Sonnet4_6
            | ClaudeModel::Sonnet4_5
            | ClaudeModel::Haiku5_5 => 1_000_000,
            // 200K context window.
            ClaudeModel::Opus4_5 | ClaudeModel::Opus4_1 | ClaudeModel::Haiku4_5 => 200_000,
            // Unknown: conservative.
            ClaudeModel::Unknown(_) => 200_000,
        }
    }

    /// Maximum output tokens. Anthropic requires an explicit `max_tokens`; this
    /// is the model ceiling (the connector streams, so large values are safe).
    pub fn max_output_tokens(&self) -> u32 {
        match self {
            ClaudeModel::Fable5_1
            | ClaudeModel::Fable5
            | ClaudeModel::Opus5_5
            | ClaudeModel::Opus5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6
            | ClaudeModel::Sonnet5_5
            | ClaudeModel::Sonnet5
            | ClaudeModel::Sonnet4_6
            | ClaudeModel::Haiku5_5 => 128_000,
            ClaudeModel::Sonnet4_5 | ClaudeModel::Opus4_5 | ClaudeModel::Haiku4_5 => 64_000,
            ClaudeModel::Opus4_1 => 32_000,
            // Unknown/custom or future slug: use a generous ceiling so the
            // streaming-safe default isn't silently truncated. If the real model
            // caps lower, Anthropic returns a loud 400 (preferable to silent loss).
            ClaudeModel::Unknown(_) => 64_000,
        }
    }

    /// USD price per 1M input tokens (for cost estimation in `UsageData`).
    /// Manually maintained; re-verify against
    /// platform.claude.com/docs/en/about-claude/pricing (SYSTEM_PATTERNS P-013).
    /// Last verified: 2026-10-07 — added Haiku 5.5 ($0.10/$0.50 up to 100k
    /// prompt tokens; see `price_multiplier`); all other rates re-checked.
    pub fn input_price_per_mtok(&self) -> f64 {
        match self {
            ClaudeModel::Fable5_1 => 10.0,
            ClaudeModel::Fable5 => 10.0,
            ClaudeModel::Opus5_5 => 4.0,
            ClaudeModel::Opus5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6 => 5.0,
            ClaudeModel::Opus4_5 => 5.0,
            ClaudeModel::Opus4_1 => 15.0,
            // $2/$10 was Sonnet 5's introductory rate through 2026-08-31 and
            // is now the standard rate (the scheduled $3/$15 rise on
            // 2026-09-01 did not happen) — verified live, not carried over
            // from the old Sonnet 4.6 ladder rung.
            ClaudeModel::Sonnet5_5 | ClaudeModel::Sonnet5 => 2.0,
            ClaudeModel::Sonnet4_6 | ClaudeModel::Sonnet4_5 => 3.0,
            ClaudeModel::Haiku5_5 => 0.10,
            ClaudeModel::Haiku4_5 => 1.0,
            ClaudeModel::Unknown(_) => 0.0,
        }
    }

    /// USD price per 1M output tokens.
    pub fn output_price_per_mtok(&self) -> f64 {
        match self {
            ClaudeModel::Fable5_1 => 50.0,
            ClaudeModel::Fable5 => 50.0,
            ClaudeModel::Opus5_5 => 20.0,
            ClaudeModel::Opus5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6 => 25.0,
            ClaudeModel::Opus4_5 => 25.0,
            ClaudeModel::Opus4_1 => 75.0,
            ClaudeModel::Sonnet5_5 | ClaudeModel::Sonnet5 => 10.0,
            ClaudeModel::Sonnet4_6 | ClaudeModel::Sonnet4_5 => 15.0,
            ClaudeModel::Haiku5_5 => 0.50,
            ClaudeModel::Haiku4_5 => 5.0,
            ClaudeModel::Unknown(_) => 0.0,
        }
    }

    /// Multiplier on every rate above (input, output, cache reads and writes)
    /// for a request whose total prompt is `prompt_tokens` (input + cache
    /// reads + cache writes). Haiku 5.5 is priced by prompt length: prompts
    /// over 100,000 tokens pay 5x ($0.50/$2.50 instead of $0.10/$0.50).
    /// Every other model is flat across its context window.
    pub fn price_multiplier(&self, prompt_tokens: u64) -> f64 {
        match self {
            ClaudeModel::Haiku5_5 if prompt_tokens > 100_000 => 5.0,
            _ => 1.0,
        }
    }

    /// USD price per 1M prompt-cache read tokens. The standard multiplier is
    /// 0.1x base input; Fable 5.1 is 0.025x and Opus 5.5 is 0.05x.
    /// Sonnet 5.5 uses the standard 0.1x ($0.20): the pricing table and its
    /// footnote say so, though one prose line on the same page (2026-10-07)
    /// says 0.05x ($0.10). Re-check when the docs settle.
    pub fn cache_read_price_per_mtok(&self) -> f64 {
        match self {
            ClaudeModel::Fable5_1 => 0.25,
            ClaudeModel::Opus5_5 => 0.20,
            other => other.input_price_per_mtok() * 0.1,
        }
    }

    /// USD price per 1M prompt-cache write tokens: 1.25x base input for the
    /// 5-minute TTL, 2x for the 1-hour TTL.
    pub fn cache_write_price_per_mtok(&self, ttl: super::config::ClaudeCacheTtl) -> f64 {
        let mult = match ttl {
            super::config::ClaudeCacheTtl::FiveMinutes => 1.25,
            super::config::ClaudeCacheTtl::OneHour => 2.0,
        };
        self.input_price_per_mtok() * mult
    }

    /// Whether a `{"role": "system"}` entry is accepted mid-conversation
    /// (as the last message). Sonnet 5 and everything older reject it.
    pub fn supports_mid_conversation_system(&self) -> bool {
        matches!(
            self,
            ClaudeModel::Opus5_5
                | ClaudeModel::Opus5
                | ClaudeModel::Opus4_8
                | ClaudeModel::Fable5
                | ClaudeModel::Fable5_1
                | ClaudeModel::Sonnet5_5
        )
    }

    /// Models that think on every request. `thinking: {type: "disabled"}` is
    /// rejected with a 400, so the connector must replay their signed
    /// thinking blocks even when the user's thinking toggle is off, and must
    /// ask for `display: "summarized"` or the thinking text arrives empty.
    pub fn thinking_always_on(&self) -> bool {
        matches!(self, ClaudeModel::Fable5_1 | ClaudeModel::Opus5_5)
    }

    /// Models whose thinking blocks are bound to the conversation that
    /// produced them (preserved thinking's history-editing check). A replayed
    /// block whose prefix changed is rejected, or dropped when the request
    /// sets `thinking.block_binding.prefix_mismatch_behavior: "drop_block"`.
    pub fn binds_thinking_to_conversation(&self) -> bool {
        matches!(
            self,
            ClaudeModel::Fable5_1 | ClaudeModel::Opus5_5 | ClaudeModel::Sonnet5_5
        )
    }

    /// Whether the model supports extended/adaptive thinking.
    /// Gates the settings toggle and the `thinking` request param.
    pub fn supports_thinking(&self) -> bool {
        match self {
            ClaudeModel::Fable5_1
            | ClaudeModel::Fable5
            | ClaudeModel::Opus5_5
            | ClaudeModel::Opus5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6
            | ClaudeModel::Opus4_5
            | ClaudeModel::Sonnet5_5
            | ClaudeModel::Sonnet5
            | ClaudeModel::Sonnet4_6
            | ClaudeModel::Sonnet4_5
            | ClaudeModel::Haiku5_5
            | ClaudeModel::Haiku4_5 => true,
            ClaudeModel::Opus4_1 => false,
            // Unknown: assume modern → supports adaptive thinking.
            ClaudeModel::Unknown(_) => true,
        }
    }

    /// All Claude models support image input.
    #[allow(dead_code)]
    pub fn supports_vision(&self) -> bool {
        true
    }

    /// Recommended/current models for the slot picker, best-first. Excludes
    /// `Unknown`. Legacy-but-active models are included so users with scoped
    /// API access can still select them; retired ones (Opus 4.1) are not.
    pub fn all_models() -> Vec<ClaudeModel> {
        vec![
            ClaudeModel::Opus5_5,
            ClaudeModel::Fable5_1,
            ClaudeModel::Sonnet5_5,
            ClaudeModel::Haiku5_5,
            ClaudeModel::Opus5,
            ClaudeModel::Sonnet5,
            ClaudeModel::Fable5,
            ClaudeModel::Opus4_8,
            ClaudeModel::Opus4_7,
            ClaudeModel::Opus4_6,
            ClaudeModel::Sonnet4_6,
            ClaudeModel::Opus4_5,
            ClaudeModel::Sonnet4_5,
            ClaudeModel::Haiku4_5,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Haiku 5.5 (verified 2026-10-07): its own slug, $0.10/$0.50 base with
    /// 0.1x cache reads, and 5x every rate once the prompt passes 100k.
    #[test]
    fn haiku_5_5_slug_prices_and_long_prompt_tier() {
        let h = ClaudeModel::from_slug("claude-haiku-5-5");
        assert_eq!(h, ClaudeModel::Haiku5_5);
        assert_eq!(h.canonical_slug(), "claude-haiku-5-5");
        assert_ne!(ClaudeModel::from_slug("claude-haiku-4-5"), ClaudeModel::Haiku5_5);
        assert!((h.input_price_per_mtok() - 0.10).abs() < 1e-12);
        assert!((h.output_price_per_mtok() - 0.50).abs() < 1e-12);
        assert!((h.cache_read_price_per_mtok() - 0.01).abs() < 1e-12);
        assert_eq!(h.price_multiplier(100_000), 1.0);
        assert_eq!(h.price_multiplier(100_001), 5.0);
        // Long-prompt tier matches the published $0.50/$2.50, $0.05 reads.
        assert!((h.input_price_per_mtok() * h.price_multiplier(200_000) - 0.50).abs() < 1e-12);
        assert!((h.output_price_per_mtok() * h.price_multiplier(200_000) - 2.50).abs() < 1e-12);
        assert!((h.cache_read_price_per_mtok() * h.price_multiplier(200_000) - 0.05).abs() < 1e-12);
        // Flat pricing everywhere else.
        assert_eq!(ClaudeModel::Opus5_5.price_multiplier(900_000), 1.0);
        assert_eq!(h.max_output_tokens(), 128_000);
        assert_eq!(h.context_window_tokens(), 1_000_000);
        assert!(h.supports_thinking() && !h.thinking_always_on());
        assert!(ClaudeModel::all_models().contains(&ClaudeModel::Haiku5_5));
    }

    /// Sonnet 5 and Sonnet 4.6 accept 128K max_tokens, like Sonnet 5.5.
    #[test]
    fn sonnet_5_and_4_6_allow_128k_output() {
        assert_eq!(ClaudeModel::Sonnet5.max_output_tokens(), 128_000);
        assert_eq!(ClaudeModel::Sonnet4_6.max_output_tokens(), 128_000);
        assert_eq!(ClaudeModel::Sonnet4_5.max_output_tokens(), 64_000);
    }

    #[test]
    fn from_slug_recognizes_aliases() {
        assert_eq!(ClaudeModel::from_slug("claude-opus-4-8"), ClaudeModel::Opus4_8);
        assert_eq!(
            ClaudeModel::from_slug("claude-sonnet-4-6"),
            ClaudeModel::Sonnet4_6
        );
        assert_eq!(ClaudeModel::from_slug("claude-fable-5"), ClaudeModel::Fable5);
    }

    /// Claude 5: a full generation past the 4.x ladder, not another point
    /// release on it. Regression coverage for the day the file had no slugs
    /// for the current flagship family at all.
    #[test]
    fn from_slug_recognizes_the_claude_5_family() {
        assert_eq!(ClaudeModel::from_slug("claude-opus-5"), ClaudeModel::Opus5);
        assert_eq!(ClaudeModel::from_slug("claude-sonnet-5"), ClaudeModel::Sonnet5);
        assert_eq!(ClaudeModel::from_slug("claude-fable-5-1"), ClaudeModel::Fable5_1);
    }

    /// "claude-fable-5-1" must resolve to Fable5_1, not fall through to the
    /// bare "claude-fable-5" prefix match — the two are separately priced.
    #[test]
    fn fable_5_1_is_distinguished_from_bare_fable_5() {
        assert_ne!(
            ClaudeModel::from_slug("claude-fable-5-1"),
            ClaudeModel::from_slug("claude-fable-5")
        );
        assert_eq!(ClaudeModel::Fable5_1.canonical_slug(), "claude-fable-5-1");
        assert_eq!(ClaudeModel::Fable5.canonical_slug(), "claude-fable-5");
    }

    /// Sonnet 5 is priced separately from the Sonnet 4.x rung it supersedes
    /// as the picker default — $2/$10, not the old $3/$15.
    #[test]
    fn sonnet_5_has_its_own_price_distinct_from_sonnet_4_6() {
        assert_eq!(ClaudeModel::Sonnet5.input_price_per_mtok(), 2.0);
        assert_eq!(ClaudeModel::Sonnet5.output_price_per_mtok(), 10.0);
        assert_eq!(ClaudeModel::Sonnet4_6.input_price_per_mtok(), 3.0);
    }

    #[test]
    fn from_slug_recognizes_dated_ids_via_prefix() {
        assert_eq!(
            ClaudeModel::from_slug("claude-haiku-4-5-20251001"),
            ClaudeModel::Haiku4_5
        );
    }

    #[test]
    fn from_slug_unknown_roundtrips() {
        let m = ClaudeModel::from_slug("claude-future-9");
        assert_eq!(m, ClaudeModel::Unknown("claude-future-9".to_string()));
        assert_eq!(m.canonical_slug(), "claude-future-9");
    }

    #[test]
    fn canonical_slug_roundtrips_for_all_models() {
        for model in ClaudeModel::all_models() {
            assert_eq!(ClaudeModel::from_slug(model.canonical_slug()), model);
        }
    }

    #[test]
    fn default_chat_slug_is_a_known_model() {
        assert_eq!(
            ClaudeModel::from_slug(ClaudeModel::DEFAULT_CHAT_SLUG),
            ClaudeModel::Opus5_5
        );
    }

    /// "claude-opus-5-5" also starts with "claude-opus-5"; it must not be
    /// parsed (and priced) as Opus 5.
    #[test]
    fn point_five_releases_are_distinguished_from_their_bare_prefix() {
        assert_eq!(ClaudeModel::from_slug("claude-opus-5-5"), ClaudeModel::Opus5_5);
        assert_eq!(ClaudeModel::from_slug("claude-sonnet-5-5"), ClaudeModel::Sonnet5_5);
        assert_eq!(ClaudeModel::from_slug("claude-opus-5"), ClaudeModel::Opus5);
        assert_eq!(ClaudeModel::from_slug("claude-sonnet-5"), ClaudeModel::Sonnet5);
    }

    #[test]
    fn september_2026_prices() {
        assert_eq!(ClaudeModel::Opus5_5.input_price_per_mtok(), 4.0);
        assert_eq!(ClaudeModel::Opus5_5.output_price_per_mtok(), 20.0);
        assert_eq!(ClaudeModel::Opus5_5.cache_read_price_per_mtok(), 0.20);
        assert_eq!(ClaudeModel::Sonnet5_5.input_price_per_mtok(), 2.0);
        assert_eq!(ClaudeModel::Sonnet5_5.output_price_per_mtok(), 10.0);
        assert_eq!(ClaudeModel::Sonnet5_5.max_output_tokens(), 128_000);
        assert_eq!(ClaudeModel::Fable5_1.cache_read_price_per_mtok(), 0.25);
        assert!((ClaudeModel::Opus5.cache_read_price_per_mtok() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn always_on_thinking_models() {
        assert!(ClaudeModel::Opus5_5.thinking_always_on());
        assert!(ClaudeModel::Fable5_1.thinking_always_on());
        assert!(!ClaudeModel::Sonnet5_5.thinking_always_on());
        assert!(!ClaudeModel::Opus5.thinking_always_on());
    }

    #[test]
    fn retired_opus_4_1_is_not_offered() {
        assert!(!ClaudeModel::all_models().contains(&ClaudeModel::Opus4_1));
        assert_eq!(ClaudeModel::from_slug("claude-opus-4-1"), ClaudeModel::Opus4_1);
    }

    #[test]
    fn metadata_is_sane() {
        let opus = ClaudeModel::Opus4_8;
        assert_eq!(opus.context_window_tokens(), 1_000_000);
        assert_eq!(opus.max_output_tokens(), 128_000);
        assert!(opus.supports_thinking());
        assert!(opus.supports_vision());
        assert!(opus.input_price_per_mtok() > 0.0);
    }

    #[test]
    fn claude_5_family_metadata_is_sane() {
        for model in [ClaudeModel::Fable5_1, ClaudeModel::Opus5, ClaudeModel::Sonnet5] {
            assert_eq!(model.context_window_tokens(), 1_000_000);
            assert!(model.supports_thinking());
            assert!(model.supports_vision());
            assert!(model.input_price_per_mtok() > 0.0);
            assert!(model.output_price_per_mtok() > model.input_price_per_mtok());
        }
    }
}
