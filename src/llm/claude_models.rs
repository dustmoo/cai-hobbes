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
//! - Modern models (Fable 5, Opus 4.6+, Sonnet 4.6) use **adaptive** thinking
//!   (`thinking: {type: "adaptive"}`); `budget_tokens` is removed and returns
//!   400. All Claude models support image input (vision).

/// Supported Claude models for metadata, pricing, and UI display.
#[derive(Debug, PartialEq, Clone)]
pub enum ClaudeModel {
    Fable5,
    Opus4_8,
    Opus4_7,
    Opus4_6,
    Opus4_5,
    Opus4_1,
    Sonnet4_6,
    Sonnet4_5,
    Haiku4_5,
    /// Any unrecognized slug — handled gracefully (conservative defaults).
    Unknown(String),
}

impl ClaudeModel {
    /// The default chat model shipped with Hobbes.
    pub const DEFAULT_CHAT_SLUG: &'static str = "claude-opus-4-8";

    /// Parse a model slug (alias or full dated ID) into a [`ClaudeModel`].
    pub fn from_slug(slug: &str) -> Self {
        let s = slug.trim();
        match s {
            _ if s.starts_with("claude-fable-5") => ClaudeModel::Fable5,
            _ if s.starts_with("claude-opus-4-8") => ClaudeModel::Opus4_8,
            _ if s.starts_with("claude-opus-4-7") => ClaudeModel::Opus4_7,
            _ if s.starts_with("claude-opus-4-6") => ClaudeModel::Opus4_6,
            _ if s.starts_with("claude-opus-4-5") => ClaudeModel::Opus4_5,
            _ if s.starts_with("claude-opus-4-1") => ClaudeModel::Opus4_1,
            _ if s.starts_with("claude-sonnet-4-6") => ClaudeModel::Sonnet4_6,
            _ if s.starts_with("claude-sonnet-4-5") => ClaudeModel::Sonnet4_5,
            _ if s.starts_with("claude-haiku-4-5") => ClaudeModel::Haiku4_5,
            other => ClaudeModel::Unknown(other.to_string()),
        }
    }

    /// The canonical alias to send to the API.
    pub fn canonical_slug(&self) -> &str {
        match self {
            ClaudeModel::Fable5 => "claude-fable-5",
            ClaudeModel::Opus4_8 => "claude-opus-4-8",
            ClaudeModel::Opus4_7 => "claude-opus-4-7",
            ClaudeModel::Opus4_6 => "claude-opus-4-6",
            ClaudeModel::Opus4_5 => "claude-opus-4-5",
            ClaudeModel::Opus4_1 => "claude-opus-4-1",
            ClaudeModel::Sonnet4_6 => "claude-sonnet-4-6",
            ClaudeModel::Sonnet4_5 => "claude-sonnet-4-5",
            ClaudeModel::Haiku4_5 => "claude-haiku-4-5",
            ClaudeModel::Unknown(slug) => slug,
        }
    }

    /// Human-readable name for the UI.
    pub fn display_name(&self) -> String {
        match self {
            ClaudeModel::Fable5 => "Claude Fable 5".to_string(),
            ClaudeModel::Opus4_8 => "Claude Opus 4.8".to_string(),
            ClaudeModel::Opus4_7 => "Claude Opus 4.7".to_string(),
            ClaudeModel::Opus4_6 => "Claude Opus 4.6".to_string(),
            ClaudeModel::Opus4_5 => "Claude Opus 4.5".to_string(),
            ClaudeModel::Opus4_1 => "Claude Opus 4.1".to_string(),
            ClaudeModel::Sonnet4_6 => "Claude Sonnet 4.6".to_string(),
            ClaudeModel::Sonnet4_5 => "Claude Sonnet 4.5".to_string(),
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
            ClaudeModel::Fable5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6
            | ClaudeModel::Sonnet4_6
            | ClaudeModel::Sonnet4_5 => 1_000_000,
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
            ClaudeModel::Fable5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6 => 128_000,
            ClaudeModel::Sonnet4_6
            | ClaudeModel::Sonnet4_5
            | ClaudeModel::Opus4_5
            | ClaudeModel::Haiku4_5 => 64_000,
            ClaudeModel::Opus4_1 => 32_000,
            // Unknown/custom or future slug: use a generous ceiling so the
            // streaming-safe default isn't silently truncated. If the real model
            // caps lower, Anthropic returns a loud 400 (preferable to silent loss).
            ClaudeModel::Unknown(_) => 64_000,
        }
    }

    /// USD price per 1M input tokens (for cost estimation in `UsageData`).
    pub fn input_price_per_mtok(&self) -> f64 {
        match self {
            ClaudeModel::Fable5 => 10.0,
            ClaudeModel::Opus4_8 | ClaudeModel::Opus4_7 | ClaudeModel::Opus4_6 => 5.0,
            ClaudeModel::Opus4_5 => 5.0,
            ClaudeModel::Opus4_1 => 15.0,
            ClaudeModel::Sonnet4_6 | ClaudeModel::Sonnet4_5 => 3.0,
            ClaudeModel::Haiku4_5 => 1.0,
            ClaudeModel::Unknown(_) => 0.0,
        }
    }

    /// USD price per 1M output tokens.
    pub fn output_price_per_mtok(&self) -> f64 {
        match self {
            ClaudeModel::Fable5 => 50.0,
            ClaudeModel::Opus4_8 | ClaudeModel::Opus4_7 | ClaudeModel::Opus4_6 => 25.0,
            ClaudeModel::Opus4_5 => 25.0,
            ClaudeModel::Opus4_1 => 75.0,
            ClaudeModel::Sonnet4_6 | ClaudeModel::Sonnet4_5 => 15.0,
            ClaudeModel::Haiku4_5 => 5.0,
            ClaudeModel::Unknown(_) => 0.0,
        }
    }

    /// Whether the model supports extended/adaptive thinking.
    /// Gates the settings toggle and the `thinking` request param.
    pub fn supports_thinking(&self) -> bool {
        match self {
            ClaudeModel::Fable5
            | ClaudeModel::Opus4_8
            | ClaudeModel::Opus4_7
            | ClaudeModel::Opus4_6
            | ClaudeModel::Opus4_5
            | ClaudeModel::Sonnet4_6
            | ClaudeModel::Sonnet4_5
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
    /// API access can still select them.
    pub fn all_models() -> Vec<ClaudeModel> {
        vec![
            ClaudeModel::Fable5,
            ClaudeModel::Opus4_8,
            ClaudeModel::Opus4_7,
            ClaudeModel::Opus4_6,
            ClaudeModel::Sonnet4_6,
            ClaudeModel::Haiku4_5,
            ClaudeModel::Opus4_5,
            ClaudeModel::Opus4_1,
            ClaudeModel::Sonnet4_5,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_slug_recognizes_aliases() {
        assert_eq!(ClaudeModel::from_slug("claude-opus-4-8"), ClaudeModel::Opus4_8);
        assert_eq!(
            ClaudeModel::from_slug("claude-sonnet-4-6"),
            ClaudeModel::Sonnet4_6
        );
        assert_eq!(ClaudeModel::from_slug("claude-fable-5"), ClaudeModel::Fable5);
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
            ClaudeModel::Opus4_8
        );
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
}
