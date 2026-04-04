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
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            chat_model: GeminiModel::Gemini3_0FlashPreview
                .canonical_slug()
                .to_string(),
            summary_model: GeminiModel::Gemini3_0FlashPreview
                .canonical_slug()
                .to_string(),
            thinking_enabled: false,
            thinking_level: default_thinking_level(),
            thinking_budget: None,
            model_slots: default_model_slots(),
            context_tuning: ContextTuningPreset::default(),
        }
    }
}

pub fn default_thinking_level() -> String {
    "high".to_string()
}

pub fn default_model_slots() -> Vec<String> {
    vec![
        GeminiModel::Gemini3_0FlashPreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_1ProPreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_1FlashLitePreview
            .canonical_slug()
            .to_string(),
        GeminiModel::Gemini3_0ProPreview
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
    /// None = use global default (4.0).
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
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct OpenAiCompatConfig {
    pub endpoint: String, // "http://localhost:11434/v1"
    pub model: String,    // "llama3.2"
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

    /// Maximum context window in tokens. Auto-populated from model discovery
    /// (vLLM's max_model_len, Ollama's num_ctx). User-overridable in settings.
    /// None = no enforcement (backwards-compatible, safe for Gemini's 1M+ context).
    #[serde(default)]
    pub max_context_tokens: Option<usize>,
    /// Per-provider context tuning overrides (None = use global defaults)
    #[serde(default)]
    pub context_tuning: ContextTuningPreset,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ClaudeConfig {
    #[serde(skip)]
    pub api_key: Option<String>,
    pub model: String, // "claude-3-7-sonnet-20250219"
    pub summary_model: Option<String>,
    pub max_tokens: Option<u32>, // Claude requires explicit max_tokens
    #[serde(default)]
    pub extended_thinking: bool,
    #[serde(default = "default_empty_model_slots")]
    pub model_slots: Vec<String>, // Provider-scoped quick-switch slots
    /// Per-provider context tuning overrides (None = use global defaults)
    #[serde(default)]
    pub context_tuning: ContextTuningPreset,
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
    fn test_claude_config_starts_with_empty_slots() {
        let config = ClaudeConfig::default();
        for (i, slot) in config.model_slots.iter().enumerate() {
            assert!(slot.is_empty(), "Claude slot {} should be empty by default", i);
        }
    }
}
