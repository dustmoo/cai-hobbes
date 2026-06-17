use super::claude_models::ClaudeModel;
use super::gemini::GeminiModel;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GeminiConfig {
    #[serde(skip)]
    pub api_key: Option<String>,
    pub chat_model: String,
    pub summary_model: String,
    #[serde(default)]
    pub thinking_enabled: bool,
    #[serde(default = "default_thinking_level")]
    pub thinking_level: String,
    #[serde(default)]
    pub thinking_budget: Option<i32>,
    #[serde(default = "default_model_slots")]
    pub model_slots: Vec<String>,
    /// Per-provider context tuning overrides (None = use global defaults)
    #[serde(default)]
    pub context_tuning: ContextTuningPreset,
    #[serde(default)]
    pub context_caching_enabled: bool,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u32,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            chat_model: GeminiModel::Gemini3_5Flash
                .canonical_slug()
                .to_string(),
            summary_model: GeminiModel::Gemini3_5Flash
                .canonical_slug()
                .to_string(),
            thinking_enabled: false,
            thinking_level: default_thinking_level(),
            thinking_budget: None,
            model_slots: default_model_slots(),
            context_tuning: ContextTuningPreset::default(),
            context_caching_enabled: false,
            cache_ttl_seconds: default_cache_ttl(),
        }
    }
}

pub fn default_thinking_level() -> String {
    "high".to_string()
}

pub fn default_cache_ttl() -> u32 {
    300
}

pub fn default_model_slots() -> Vec<String> {
    vec![
        GeminiModel::Gemini3_5Flash
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_1ProPreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_1FlashLitePreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_0FlashPreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini2_5Flash.canonical_slug().to_string(),
        "".to_string(), // Slot 6
        "".to_string(), // Slot 7
        "".to_string(), // Slot 8
        "".to_string(), // Slot 9
        "".to_string(), // Slot 10
    ]
}

fn default_empty_model_slots() -> Vec<String> {
    vec!["".to_string(); 10]
}

/// Default Claude quick-switch slots: current models first, remaining slots empty.
pub fn default_claude_model_slots() -> Vec<String> {
    let mut slots: Vec<String> = vec![
        ClaudeModel::Opus4_8.canonical_slug().to_string(),
        ClaudeModel::Sonnet4_6.canonical_slug().to_string(),
        ClaudeModel::Haiku4_5.canonical_slug().to_string(),
        ClaudeModel::Opus4_7.canonical_slug().to_string(),
    ];
    slots.resize(10, String::new());
    slots
}

/// Default max output tokens for Claude requests. Anthropic requires an explicit
/// `max_tokens`; this streaming-safe default works across all current models and
/// is user-editable in settings.
pub fn default_claude_max_tokens() -> Option<u32> {
    Some(32_000)
}


/// Per-provider overrides for context tuning.
/// All fields are Option — None means "use global Settings default".
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ContextTuningPreset {
    pub chat_history_length: Option<usize>,
    pub max_tool_output_length: Option<usize>,
    pub max_active_tool_output_length: Option<usize>,
    pub max_summary_chars: Option<usize>,
    pub max_entity_count: Option<usize>,
    /// Convert tool results from JSON to compact markdown for smaller models.
    /// None = use provider default (true for OpenAI-compat, false for others).
    pub compact_tool_results: Option<bool>,
    /// Fraction of the context window to allocate for active tool results (0.0–1.0).
    /// None = use global default (0.30 = 30%).
    pub tool_result_budget_ratio: Option<f64>,
    /// Characters per token ratio for context budget calculations.
    /// English prose ≈ 4.0, CJK/code-heavy content ≈ 2.0.
    /// None = use global default (3.0).
    pub chars_per_token: Option<f64>,
    /// Fraction of remaining context allocated to the currently active tool result
    pub active_result_budget_ratio: Option<f64>,
    /// Reserved fraction of context window to prevent overflow
    pub context_safety_margin: Option<f64>,
    /// Maximum fraction of context window for system instructions
    pub system_prompt_budget_ratio: Option<f64>,
}

impl ContextTuningPreset {
    /// Clamp a budget ratio to the valid 5–95% range.
    /// Centralizes the magic numbers used in prompt_builder and settings_panel.
    pub fn clamp_budget_ratio(ratio: f64) -> f64 {
        ratio.clamp(0.05, 0.95)
    }
}
/// A watch word pattern that triggers automatic continuation when detected
/// in the model's output. Useful for models that stall mid-sentence
/// (e.g., Qwen 3.6 halts after "Let me ...").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchWord {
    /// The pattern to match (case-insensitive). Matched via `contains`
    /// against the model's final text output.
    pub pattern: String,
    /// Custom instruction to inject as a user message when this watch word
    /// fires. If empty, a default continuation instruction is used.
    #[serde(default)]
    pub instruction: String,
}

impl WatchWord {
    /// Check whether `text` contains this watch word (case-insensitive).
    pub fn matches(&self, text: &str) -> bool {
        if self.pattern.is_empty() {
            return false;
        }
        text.to_lowercase().contains(&self.pattern.to_lowercase())
    }

    /// Return the instruction to inject, falling back to a sensible default.
    pub fn effective_instruction(&self) -> &str {
        if self.instruction.trim().is_empty() {
            "You stopped mid-response without completing the action you described. Do NOT repeat what you already said — proceed directly with the actual tool call or action you were about to perform."
        } else {
            &self.instruction
        }
    }
}

/// Default watch words shipped with Hobbes — covers the most common stall
/// patterns observed with Qwen and similar models.
pub fn default_watch_words() -> Vec<WatchWord> {
    vec![WatchWord {
        pattern: "Let me ".to_string(),
        instruction: String::new(), // Uses the default effective_instruction
    }]
}

/// Default maximum number of watch-word-triggered recoveries per user turn.
/// Resets when the user sends a new message.
pub fn default_max_watch_word_recoveries() -> u32 {
    3
}

/// Default max response length (in characters) for watch word detection.
/// Responses longer than this are assumed complete and skip watch word checks.
pub fn default_watch_word_max_response_chars() -> usize {
    500
}

/// Which OpenAI API surface to target. The newest OpenAI reasoning models
/// (gpt-5 / o-series) are served only by the Responses API; everything else
/// (local servers, OpenRouter, older OpenAI models) uses Chat Completions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiStyle {
    /// Use the Responses API for OpenAI's gpt-5/o-series on api.openai.com;
    /// Chat Completions everywhere else.
    #[default]
    Auto,
    /// Always use Chat Completions (`/v1/chat/completions`).
    ChatCompletions,
    /// Always use the Responses API (`/v1/responses`).
    Responses,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiCompatConfig {
    pub endpoint: String, // "http://localhost:11434/v1"
    pub model: String,    // "llama3.2"
    /// Which OpenAI API surface to target (Auto routes by endpoint + model).
    #[serde(default)]
    pub api_style: ApiStyle,
    #[serde(skip)]
    pub api_key: Option<String>, // Optional for local providers
    pub summary_model: Option<String>, // Falls back to model
    #[serde(default = "default_empty_model_slots")]
    pub model_slots: Vec<String>, // Provider-scoped quick-switch slots
    /// Enable sending tool/function definitions to the provider.
    /// Most local servers (vLLM, Ollama, llama.cpp) require special flags
    /// (e.g. --enable-auto-tool-choice) or don't support tools at all.
    /// Default: false — tools are NOT sent unless explicitly opted in.
    #[serde(default)]
    pub tools_enabled: bool,

    /// Enable thinking/reasoning mode for models that support it (e.g. Gemma 4, Qwen3).
    /// When enabled, sends `chat_template_kwargs: {"enable_thinking": true}` to vLLM
    /// so the server activates the reasoning parser and separates thinking from response.
    /// Default: false.
    #[serde(default)]
    pub thinking_enabled: bool,

    /// Maximum context window in tokens. Auto-populated from model discovery
    /// (vLLM's max_model_len, Ollama's num_ctx). User-overridable in settings.
    /// None = no enforcement (backwards-compatible, safe for Gemini's 1M+ context).
    #[serde(default)]
    pub max_context_tokens: Option<usize>,
    /// Per-provider context tuning overrides (None = use global defaults)
    #[serde(default)]
    pub context_tuning: ContextTuningPreset,

    /// Master toggle for watch word auto-recovery.
    /// When enabled, the harness scans completed AI responses for stall
    /// patterns and automatically triggers a continuation turn.
    #[serde(default)]
    pub watch_words_enabled: bool,

    /// Watch word patterns that trigger auto-recovery when detected in output.
    #[serde(default = "default_watch_words")]
    pub watch_words: Vec<WatchWord>,

    /// Maximum number of watch-word-triggered recoveries allowed per user
    /// turn before halting. Prevents infinite loops if the model keeps
    /// producing stalled output. Resets when the user sends a new message.
    #[serde(default = "default_max_watch_word_recoveries")]
    pub max_watch_word_recoveries: u32,

    /// Watch words are only checked when the AI's response is shorter than
    /// this character count. Long responses are almost certainly complete
    /// even if they happen to contain a watch word pattern. This prevents
    /// false positives on legitimate text like "Let me summarize...".
    /// Default: 500 characters.
    #[serde(default = "default_watch_word_max_response_chars")]
    pub watch_word_max_response_chars: usize,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            model: String::new(),
            api_style: ApiStyle::default(),
            api_key: None,
            summary_model: None,
            model_slots: default_empty_model_slots(),
            tools_enabled: false,
            thinking_enabled: false,
            max_context_tokens: None,
            context_tuning: ContextTuningPreset::default(),
            watch_words_enabled: false,
            watch_words: default_watch_words(),
            max_watch_word_recoveries: default_max_watch_word_recoveries(),
            watch_word_max_response_chars: default_watch_word_max_response_chars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaudeConfig {
    #[serde(skip)]
    pub api_key: Option<String>,
    pub model: String, // canonical Anthropic alias, e.g. "claude-opus-4-8"
    pub summary_model: Option<String>,
    #[serde(default = "default_claude_max_tokens")]
    pub max_tokens: Option<u32>, // Claude requires explicit max_tokens
    #[serde(default)]
    pub extended_thinking: bool,
    #[serde(default = "default_claude_model_slots")]
    pub model_slots: Vec<String>, // Provider-scoped quick-switch slots
    /// Per-provider context tuning overrides (None = use global defaults)
    #[serde(default)]
    pub context_tuning: ContextTuningPreset,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            model: ClaudeModel::DEFAULT_CHAT_SLUG.to_string(),
            // Default summaries to a cheaper model; falls back to `model` if unset.
            summary_model: Some(ClaudeModel::Haiku4_5.canonical_slug().to_string()),
            max_tokens: default_claude_max_tokens(),
            extended_thinking: false,
            model_slots: default_claude_model_slots(),
            context_tuning: ContextTuningPreset::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_model_slots_has_gemini_slugs() {
        let slots = default_model_slots();
        assert_eq!(slots.len(), 10);
        // First 5 should be non-empty Gemini slugs
        assert!(!slots[0].is_empty(), "Slot 0 should have a Gemini model slug");
        assert!(slots[0].contains("gemini"), "Slot 0 should be a Gemini model");
        // Last 5 should be empty
        for i in 5..10 {
            assert!(slots[i].is_empty(), "Slot {} should be empty", i);
        }
    }

    #[test]
    fn test_empty_model_slots_for_openai_and_claude() {
        let slots = default_empty_model_slots();
        assert_eq!(slots.len(), 10);
        for (i, slot) in slots.iter().enumerate() {
            assert!(slot.is_empty(), "Slot {} should be empty for non-Gemini providers", i);
        }
    }

    #[test]
    fn test_gemini_config_defaults() {
        let config = GeminiConfig::default();
        assert!(!config.chat_model.is_empty());
        assert!(!config.summary_model.is_empty());
        assert!(!config.thinking_enabled);
        assert_eq!(config.thinking_level, "high");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn test_openai_compat_config_starts_with_empty_slots() {
        let config = OpenAiCompatConfig::default();
        for (i, slot) in config.model_slots.iter().enumerate() {
            assert!(slot.is_empty(), "OpenAI-compat slot {} should be empty by default", i);
        }
    }

    #[test]
    fn test_claude_config_defaults() {
        let config = ClaudeConfig::default();
        assert_eq!(config.model_slots.len(), 10);
        // First slots are pre-filled with current Claude models.
        assert_eq!(config.model_slots[0], "claude-opus-4-8");
        assert!(config.model_slots[1].contains("claude"));
        // Trailing slots remain empty.
        for i in 4..10 {
            assert!(config.model_slots[i].is_empty(), "Claude slot {} should be empty", i);
        }
        // Sensible non-empty default model + required max_tokens.
        assert_eq!(config.model, "claude-opus-4-8");
        assert!(config.max_tokens.is_some());
    }

    #[test]
    fn test_watch_word_matches_case_insensitive() {
        let ww = WatchWord {
            pattern: "Let me ".to_string(),
            instruction: String::new(),
        };
        assert!(ww.matches("Let me check that for you"));
        assert!(ww.matches("let me check that for you"));
        assert!(ww.matches("OK, LET ME check that"));
        assert!(!ww.matches("I'll let you know"));
    }

    #[test]
    fn test_watch_word_contains_not_ends_with() {
        let ww = WatchWord {
            pattern: "Let me ".to_string(),
            instruction: String::new(),
        };
        // Pattern appears in the middle — should match
        assert!(ww.matches("I will now... Let me think about this for a moment."));
        // Pattern at the end
        assert!(ww.matches("Let me "));
    }

    #[test]
    fn test_watch_word_empty_pattern_never_matches() {
        let ww = WatchWord {
            pattern: String::new(),
            instruction: String::new(),
        };
        assert!(!ww.matches("anything at all"));
        assert!(!ww.matches(""));
    }

    #[test]
    fn test_watch_word_effective_instruction_default() {
        let ww = WatchWord {
            pattern: "test".to_string(),
            instruction: String::new(),
        };
        assert!(ww.effective_instruction().contains("stopped mid-response"));
    }

    #[test]
    fn test_watch_word_effective_instruction_custom() {
        let ww = WatchWord {
            pattern: "test".to_string(),
            instruction: "Custom recovery instruction".to_string(),
        };
        assert_eq!(ww.effective_instruction(), "Custom recovery instruction");
    }

    #[test]
    fn test_default_watch_words_not_empty() {
        let words = default_watch_words();
        assert!(!words.is_empty(), "Should ship with at least one default watch word");
        assert!(words[0].matches("Let me check that"));
    }

    #[test]
    fn test_openai_compat_default_watch_words() {
        let config = OpenAiCompatConfig::default();
        assert!(!config.watch_words_enabled, "Watch words should be disabled by default");
        assert!(!config.watch_words.is_empty(), "Should have default watch words pre-populated");
        assert_eq!(config.max_watch_word_recoveries, 3);
    }

    #[test]
    fn test_watch_word_serialization_roundtrip() {
        let ww = WatchWord {
            pattern: "Let me ".to_string(),
            instruction: "Continue please".to_string(),
        };
        let json = serde_json::to_string(&ww).unwrap();
        let deserialized: WatchWord = serde_json::from_str(&json).unwrap();
        assert_eq!(ww, deserialized);
    }
}
