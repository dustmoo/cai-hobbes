use crate::context::permissions::{PermissionSettings, ToolCategory};
pub use crate::llm::{ClaudeConfig, GeminiConfig, OpenAiCompatConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LlmProvider {
    #[default]
    Gemini,
    OpenAiCompat,
    Claude,
}

impl LlmProvider {
    /// Human-readable name for UI display
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::OpenAiCompat => "OpenAI Compatible",
            Self::Claude => "Claude",
        }
    }

    /// Keychain key name for this provider's API key
    pub fn keychain_key(&self) -> &'static str {
        match self {
            Self::Gemini => "gemini_api_key",
            Self::OpenAiCompat => "openai_compat_api_key",
            Self::Claude => "claude_api_key",
        }
    }

    /// All provider variants for UI iteration
    /// NOTE: Claude is excluded from this version's UI — the stub connector isn't ready.
    /// The LlmProvider::Claude enum variant remains for structural completeness.
    pub fn all_variants() -> &'static [LlmProvider] {
        &[Self::Gemini, Self::OpenAiCompat]
    }
}

/// Application color theme
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HotkeySettings {
    #[serde(default = "default_toggle_settings")]
    pub toggle_settings: String,
    #[serde(default = "default_toggle_history")]
    pub toggle_history: String,
    #[serde(default = "default_toggle_mcp")]
    pub toggle_mcp: String,
    #[serde(default = "default_toggle_profile")]
    pub toggle_profile: String,
    #[serde(default = "default_toggle_attachments")]
    pub toggle_attachments: String,
    #[serde(default = "default_toggle_tray")]
    pub toggle_tray: String,
    #[serde(default = "default_toggle_new_chat")]
    pub toggle_new_chat: String,
    #[serde(default = "default_toggle_new_chat_with_memory")]
    pub toggle_new_chat_with_memory: String,
    #[serde(default = "default_toggle_scroll_to_bottom")]
    pub toggle_scroll_to_bottom: String,
    #[serde(default = "default_toggle_focus_chat")]
    pub toggle_focus_chat: String,
    #[serde(default = "default_submit_chat")]
    pub submit_chat: String,
    #[serde(default = "default_cancel_generation")]
    pub cancel_generation: String,
    #[serde(default = "default_switch_tab_1")]
    pub switch_tab_1: String,
    #[serde(default = "default_switch_tab_2")]
    pub switch_tab_2: String,
    #[serde(default = "default_switch_tab_3")]
    pub switch_tab_3: String,
    #[serde(default = "default_switch_tab_4")]
    pub switch_tab_4: String,
    #[serde(default = "default_switch_tab_5")]
    pub switch_tab_5: String,
    #[serde(default = "default_switch_tab_6")]
    pub switch_tab_6: String,
    #[serde(default = "default_switch_tab_7")]
    pub switch_tab_7: String,
    #[serde(default = "default_switch_tab_8")]
    pub switch_tab_8: String,
    #[serde(default = "default_switch_tab_9")]
    pub switch_tab_9: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            toggle_settings: default_toggle_settings(),
            toggle_history: default_toggle_history(),
            toggle_mcp: default_toggle_mcp(),
            toggle_profile: default_toggle_profile(),
            toggle_attachments: default_toggle_attachments(),
            toggle_tray: default_toggle_tray(),
            toggle_new_chat: default_toggle_new_chat(),
            toggle_new_chat_with_memory: default_toggle_new_chat_with_memory(),
            toggle_scroll_to_bottom: default_toggle_scroll_to_bottom(),
            toggle_focus_chat: default_toggle_focus_chat(),
            submit_chat: default_submit_chat(),
            cancel_generation: default_cancel_generation(),
            switch_tab_1: default_switch_tab_1(),
            switch_tab_2: default_switch_tab_2(),
            switch_tab_3: default_switch_tab_3(),
            switch_tab_4: default_switch_tab_4(),
            switch_tab_5: default_switch_tab_5(),
            switch_tab_6: default_switch_tab_6(),
            switch_tab_7: default_switch_tab_7(),
            switch_tab_8: default_switch_tab_8(),
            switch_tab_9: default_switch_tab_9(),
        }
    }
}

fn default_toggle_settings() -> String {
    "CmdOrCtrl+,".to_string()
}
fn default_toggle_history() -> String {
    "CmdOrCtrl+Shift+H".to_string()
}
fn default_toggle_mcp() -> String {
    "CmdOrCtrl+Shift+M".to_string()
}
fn default_toggle_profile() -> String {
    "CmdOrCtrl+Shift+P".to_string()
}
fn default_toggle_attachments() -> String {
    "CmdOrCtrl+Shift+A".to_string()
}
fn default_active_result_budget_ratio() -> f64 { 0.60 }
fn default_context_safety_margin() -> f64 { 0.10 }
fn default_system_prompt_budget_ratio() -> f64 { 0.20 }
fn default_toggle_tray() -> String {
    "CmdOrCtrl+Shift+Space".to_string()
}
fn default_toggle_new_chat() -> String {
    "CmdOrCtrl+Shift+N".to_string()
}
fn default_toggle_new_chat_with_memory() -> String {
    "CmdOrCtrl+Alt+N".to_string()
}
fn default_toggle_focus_chat() -> String {
    "CmdOrCtrl+/".to_string()
}
fn default_toggle_scroll_to_bottom() -> String {
    "CmdOrCtrl+Shift+ArrowDown".to_string()
}
fn default_submit_chat() -> String {
    "CmdOrCtrl+Enter".to_string()
}
fn default_cancel_generation() -> String {
    "CmdOrCtrl+.".to_string()
}

macro_rules! default_switch_tab {
    ($($n:literal => $fn_name:ident),+ $(,)?) => {
        $(
            fn $fn_name() -> String {
                format!("CmdOrCtrl+Shift+{}", $n)
            }
        )+
    };
}

default_switch_tab! {
    1 => default_switch_tab_1,
    2 => default_switch_tab_2,
    3 => default_switch_tab_3,
    4 => default_switch_tab_4,
    5 => default_switch_tab_5,
    6 => default_switch_tab_6,
    7 => default_switch_tab_7,
    8 => default_switch_tab_8,
    9 => default_switch_tab_9,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub active_llm: LlmProvider,
    pub gemini_config: GeminiConfig,
    #[serde(default)]
    pub openai_compat_config: OpenAiCompatConfig,
    #[serde(default)]
    pub claude_config: ClaudeConfig,
    pub persona: String,
    pub user_name: Option<String>,
    pub force_tool_use_instruction: Option<String>,
    pub project_folder: Option<String>,
    pub chat_history_length: usize,
    pub show_tray_icon: bool,
    pub permission_settings: PermissionSettings,
    #[serde(default = "default_true")]
    pub confirm_on_delete: bool,
    #[serde(default = "default_true")]
    pub confirm_on_save: bool,
    #[serde(default = "default_true")]
    pub confirm_on_message_delete: bool,
    #[serde(default = "default_true")]
    pub confirm_forget_memory: bool,
    #[serde(skip)]
    pub smithery_api_key: Option<String>,
    #[serde(default)]
    pub preferred_mcp_source: McpSource,
    /// How API keys are stored: Biometric (device-only) or iCloud sync
    #[serde(default)]
    pub keychain_storage_mode: KeychainStorageMode,
    #[serde(default)]
    pub hotkeys: HotkeySettings,
    #[serde(default = "default_max_tool_output_length")]
    pub max_tool_output_length: usize,
    #[serde(default = "default_max_active_tool_output_length")]
    pub max_active_tool_output_length: usize,
    #[serde(default = "default_max_summary_chars")]
    pub max_summary_chars: usize,
    #[serde(default = "default_max_entity_count")]
    pub max_entity_count: usize,
    /// Fraction of the context window to allocate for active tool results (0.0–1.0).
    /// Default: 0.30 (30%). Used by `effective_tool_result_limit` for providers with
    /// finite context windows (OpenAI-compat, Claude).
    #[serde(default = "default_tool_result_budget_ratio")]
    pub tool_result_budget_ratio: f64,
    #[serde(default = "default_active_result_budget_ratio")]
    pub active_result_budget_ratio: f64,
    #[serde(default = "default_context_safety_margin")]
    pub context_safety_margin: f64,
    #[serde(default = "default_system_prompt_budget_ratio")]
    pub system_prompt_budget_ratio: f64,
    #[serde(default)]
    pub image_generation_config: ImageGenerationConfig,
    // Composio profiles
    #[serde(default)]
    pub composio_profiles: Vec<ComposioProfile>,
    pub active_composio_profile: Option<String>,
    // Legacy fields for migration (will be removed in future)
    #[serde(skip_serializing)]
    pub composio_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub composio_entity_id: Option<String>,
    #[serde(skip)]
    pub composio_api_key: Option<String>,
    #[serde(skip_serializing)]
    pub composio_user_id: Option<String>,
    /// Application color theme (Dark, Light, or System)
    #[serde(default)]
    pub theme: Theme,
    /// Version of Terms of Service the user accepted (e.g., "1.0")
    /// None means TOS has never been accepted. Compare against CURRENT_TOS_VERSION.
    #[serde(default)]
    pub tos_accepted_version: Option<String>,
    /// Custom icons/emojis for each model (key = model slug, value = emoji)
    #[serde(default)]
    pub model_icons: HashMap<String, String>,
    /// Whether background/proactive summarization is enabled.
    /// When false, both the idle-timer and proactive (tool-loop) summarizers are skipped.
    #[serde(default = "default_true")]
    pub enable_summarization: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ImageGenerationConfig {
    /// The Gemini model slug used for image generation (e.g. "gemini-2.0-flash-exp-image-generation")
    pub model: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum McpSource {
    Smithery,
    Composio,
}

impl Default for McpSource {
    fn default() -> Self {
        Self::Composio
    }
}

/// How API keys should be stored in the keychain
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum KeychainStorageMode {
    /// Biometric protection (Touch ID/passcode). Device-only, cannot sync.
    /// Only available with provisioning profile (App Store/TestFlight).
    Biometric,
    /// iCloud sync. Keys sync across devices, no biometric protection.
    /// Only available with provisioning profile (App Store/TestFlight).
    ICloudSync,
    /// Local keychain storage. No biometric, no sync.
    /// For PRO/Developer ID builds without provisioning profile.
    LocalKeychain,
}

impl Default for KeychainStorageMode {
    fn default() -> Self {
        // Default based on environment - PRO builds should use LocalKeychain
        if is_sandboxed() {
            Self::Biometric
        } else {
            Self::LocalKeychain
        }
    }
}

/// Check if the app is running in a sandboxed environment (App Store/TestFlight build).
///
/// Detection methods (in order):
/// 1. Check if HOME is within an App Sandbox container path
/// 2. Fallback: Check for embedded.provisionprofile (works during local dev builds)
///
/// Apple strips embedded.provisionprofile during TestFlight/App Store distribution,
/// so we can't rely on that file alone for production builds.
pub fn is_sandboxed() -> bool {
    #[cfg(target_os = "macos")]
    {
        // Primary check: Sandbox container path detection
        // Sandboxed apps have HOME set to ~/Library/Containers/{bundle-id}/Data
        if let Ok(home) = std::env::var("HOME") {
            if home.contains("/Library/Containers/") {
                return true;
            }
        }

        // Fallback: Check for provisioning profile (local dev builds)
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .map(|p| p.join("embedded.provisionprofile").exists())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Returns the branded app name based on distribution variant.
/// - Sandboxed (App Store): "Hobbes"
/// - Unsandboxed (Pro/Direct Download): "Hobbes Pro"
pub fn get_app_name() -> &'static str {
    if is_sandboxed() {
        "Hobbes"
    } else {
        "Hobbes Pro"
    }
}

/// Attribution line for About screens (applies to both variants)
pub const APP_ATTRIBUTION: &str =
    "Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families.";

// ============================================================================
// TERMS OF SERVICE VERSION
// ============================================================================
// Bump this version string when assets/legal/terms_of_service.md changes.
// The version should match the "Version X.X" in the markdown file header.
// Users who accepted an older version will see the TOS screen again.
// ============================================================================
pub const CURRENT_TOS_VERSION: &str = "1.2";

/// How a Composio toolkit's tools are made available to the AI.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum ToolkitLoadMode {
    /// All tools loaded upfront into every Gemini request
    Loaded,
    /// Tools discovered via meta-tools, then injected dynamically (default)
    #[default]
    OnDemand,
    /// Toolkit completely hidden from AI
    Excluded,
}

/// Configuration for a single Composio toolkit's loading behavior
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ComposioToolkitConfig {
    /// Toolkit slug (e.g., "gmail", "clickup", "github")
    pub slug: String,
    /// Human-readable display name
    pub display_name: String,
    /// Number of tools in this toolkit (cached for UI display)
    #[serde(default)]
    pub tool_count: usize,
    /// DEPRECATED: Legacy boolean, kept for backward-compatible deserialization.
    /// New code should use `load_mode` instead.
    #[serde(default, skip_serializing)]
    pub force_load: bool,
    /// How this toolkit's tools are loaded (replaces `force_load`).
    #[serde(default)]
    pub load_mode: ToolkitLoadMode,
}

impl ComposioToolkitConfig {
    /// Resolve the effective load mode, accounting for legacy `force_load` migration.
    /// If `load_mode` is the default (OnDemand) AND `force_load` is true, treat as `Loaded`.
    pub fn effective_load_mode(&self) -> ToolkitLoadMode {
        if self.force_load && self.load_mode == ToolkitLoadMode::OnDemand {
            ToolkitLoadMode::Loaded
        } else {
            self.load_mode
        }
    }
}

/// A Composio profile containing connection settings for one Composio account
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ComposioProfile {
    #[serde(default = "default_uuid")]
    pub id: String,
    pub name: String,
    pub base_url: Option<String>,
    pub entity_id: Option<String>,
    pub user_id: Option<String>,
    // API key is stored in SecretManager, not serialized
    #[serde(skip)]
    pub api_key: Option<String>,
    /// Per-toolkit loading configuration (upfront vs lazy/on-demand)
    #[serde(default)]
    pub toolkit_configs: Vec<ComposioToolkitConfig>,
    /// Profile display color (Tailwind class)
    #[serde(default = "default_profile_color")]
    pub color: String,
    /// Chrome profile directory name for scoped auth URL launching (e.g., "Default", "Profile 1")
    /// Prevents OAuth credentials from landing in the wrong Chrome profile.
    #[serde(default)]
    pub chrome_profile_directory: Option<String>,
}

fn default_profile_color() -> String {
    "bg-blue-600".to_string()
}

fn default_uuid() -> String {
    Uuid::new_v4().to_string()
}

impl Default for ComposioProfile {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name: "Default".to_string(),
            base_url: None,
            entity_id: None,
            user_id: None,
            api_key: None,
            toolkit_configs: Vec::new(),
            color: default_profile_color(),
            chrome_profile_directory: None,
        }
    }
}

impl ComposioProfile {
    /// Get slugs of toolkits configured for "Loaded" mode (all tools upfront).
    pub fn get_force_load_toolkit_slugs(&self) -> Vec<String> {
        self.toolkit_configs
            .iter()
            .filter(|c| c.effective_load_mode() == ToolkitLoadMode::Loaded)
            .map(|c| c.slug.clone())
            .collect()
    }

    /// Check if the profile is fully configured (has both User ID and API Key)
    pub fn is_fully_configured(&self) -> bool {
        self.user_id.as_ref().is_some_and(|s| !s.is_empty())
            && self.api_key.as_ref().is_some_and(|s| !s.is_empty())
    }
}

impl Default for Settings {
    fn default() -> Self {
        let mut granular_permissions = HashMap::new();
        granular_permissions.insert(ToolCategory::Mcp, true);

        Self {
            active_result_budget_ratio: 0.60,
            context_safety_margin: 0.10,
            system_prompt_budget_ratio: 0.20,
            active_llm: LlmProvider::Gemini,
            gemini_config: GeminiConfig::default(),
            openai_compat_config: OpenAiCompatConfig::default(),
            claude_config: ClaudeConfig::default(),
            persona: "You are Hobbes. Be a direct, clear, and radically candid partner. Function as an exocortex, matching the user's communication style. Your default tone is that of a professional friend.".to_string(),
            user_name: None,
            force_tool_use_instruction: Some("You must always use the provided tools to answer the user's request, even if you think you know the answer. Do not answer from your own knowledge base when tools are available. When using the fetch tool, you MUST provide markdown links as sources. When you use tools you MUST use the information from the tool, if you don't have it, look up fresh information instead of guessing.".to_string()),
            project_folder: None,
            chat_history_length: 8,
            show_tray_icon: true,
            permission_settings: PermissionSettings {
                auto_approval_enabled: true,
                granular_permissions,
                mcp_server_permissions: HashMap::new(),
                skill_permissions: HashMap::new(),
                max_ai_turns: 25,
            },
            confirm_on_delete: true,
            confirm_on_save: true,
            confirm_on_message_delete: true,
            confirm_forget_memory: true,
            smithery_api_key: None,
            preferred_mcp_source: McpSource::default(),
            keychain_storage_mode: KeychainStorageMode::default(),
            max_tool_output_length: default_max_tool_output_length(),
            max_active_tool_output_length: default_max_active_tool_output_length(),
            max_summary_chars: default_max_summary_chars(),
            max_entity_count: default_max_entity_count(),
            tool_result_budget_ratio: default_tool_result_budget_ratio(),
            image_generation_config: ImageGenerationConfig::default(),
            composio_profiles: Vec::new(),
            hotkeys: HotkeySettings::default(),
            active_composio_profile: None,
            // Legacy fields for migration
            composio_base_url: None,
            composio_entity_id: None,
            composio_api_key: None,
            composio_user_id: None,
            theme: Theme::default(),
            tos_accepted_version: None,
            model_icons: HashMap::new(),
            enable_summarization: true,
        }
    }
}

/// Get a default icon for a model based on its slug
pub fn get_default_model_icon(model_slug: &str) -> String {
    let slug = model_slug.to_lowercase();
    // Match specific variants first for visual distinction

    // --- Gemini family ---
    if slug.contains("experimental") {
        "🧪".to_string()
    } else if slug.contains("gemini-3.1") {
        "✨".to_string()
    } else if slug.contains("image") {
        "🎨".to_string()
    } else if slug.contains("lite") {
        "🪶".to_string()
    } else if slug.contains("flash-001") || slug.contains("flash_001") {
        "💫".to_string()
    } else if slug.contains("2.0-flash") || slug.contains("2.0_flash") {
        "🔥".to_string()
    } else if slug.contains("2.5-flash") || slug.contains("2.5_flash") {
        "⚡".to_string()
    } else if slug.contains("2.5-pro") || slug.contains("2.5_pro") {
        "🧠".to_string()
    } else if slug.contains("nano") {
        "🔬".to_string()
    } else if slug.contains("gemma") {
        "💠".to_string()
    // --- OpenAI family ---
    } else if slug.contains("o3") || slug.contains("o1") {
        "🔮".to_string() // reasoning models
    } else if slug.contains("gpt-4.1") {
        "🏆".to_string()
    } else if slug.contains("gpt-4o") {
        "💎".to_string()
    } else if slug.contains("gpt-4") {
        "🌟".to_string()
    } else if slug.contains("gpt-3.5") || slug.contains("gpt-35") {
        "💬".to_string()
    // --- Open-source families ---
    } else if slug.contains("qwen") {
        "🐼".to_string()
    } else if slug.contains("deepseek") {
        "🔭".to_string()
    } else if slug.contains("llama") {
        "🦙".to_string()
    } else if slug.contains("mistral") || slug.contains("mixtral") {
        "🌬️".to_string()
    } else if slug.contains("codestral") {
        "💻".to_string()
    } else if slug.contains("phi") {
        "🔬".to_string()
    } else if slug.contains("command") {
        "⌘".to_string() // Cohere Command
    // --- Generic fallbacks ---
    } else if slug.contains("pro") {
        "💎".to_string()
    } else if slug.contains("flash") {
        "⚡".to_string()
    } else {
        "🤖".to_string()
    }
}

/// Get the fixed icon for a model slot position (0-indexed)
/// Each slot has a unique, visually distinct icon regardless of the model assigned to it.
pub fn get_slot_icon(slot_index: usize) -> String {
    match slot_index {
        0 => "⚡",
        1 => "🧠",
        2 => "🔥",
        3 => "🪶",
        4 => "💎",
        5 => "🎨",
        6 => "🧪",
        7 => "💫",
        8 => "🔬",
        _ => "🤖",
    }
    .to_string()
}

impl Settings {
    pub fn active_model_slots(&self) -> Vec<String> {
        match self.active_llm {
            LlmProvider::Gemini => self.gemini_config.model_slots.clone(),
            LlmProvider::OpenAiCompat => self.openai_compat_config.model_slots.clone(),
            LlmProvider::Claude => self.claude_config.model_slots.clone(),
        }
    }

    /// Resolve the effective context tuning for the current active provider.
    /// Provider preset overrides → Global settings → Compiled defaults.
    pub fn effective_context_tuning(&self) -> ResolvedContextTuning {
        use crate::llm::config::ContextTuningPreset;
        let preset: &ContextTuningPreset = match self.active_llm {
            LlmProvider::Gemini => &self.gemini_config.context_tuning,
            LlmProvider::OpenAiCompat => &self.openai_compat_config.context_tuning,
            LlmProvider::Claude => &self.claude_config.context_tuning,
        };

        ResolvedContextTuning {
            chat_history_length: preset.chat_history_length
                .unwrap_or(self.chat_history_length),
            max_tool_output_length: preset.max_tool_output_length
                .unwrap_or(self.max_tool_output_length),
            max_active_tool_output_length: preset.max_active_tool_output_length
                .unwrap_or(self.max_active_tool_output_length),
            max_summary_chars: preset.max_summary_chars
                .unwrap_or(self.max_summary_chars),
            max_entity_count: preset.max_entity_count
                .unwrap_or(self.max_entity_count),
            compact_tool_results: preset.compact_tool_results.unwrap_or(
                matches!(self.active_llm, LlmProvider::OpenAiCompat)
            ),
            tool_result_budget_ratio: preset.tool_result_budget_ratio
                .unwrap_or(self.tool_result_budget_ratio),
            active_result_budget_ratio: preset.active_result_budget_ratio
                .unwrap_or(self.active_result_budget_ratio),
            context_safety_margin: preset.context_safety_margin
                .unwrap_or(self.context_safety_margin),
            system_prompt_budget_ratio: preset.system_prompt_budget_ratio
                .unwrap_or(self.system_prompt_budget_ratio),
            chars_per_token: preset.chars_per_token
                .unwrap_or(DEFAULT_CHARS_PER_TOKEN),
        }
    }

    pub fn active_chat_model(&self) -> String {
        match self.active_llm {
            LlmProvider::Gemini => self.gemini_config.chat_model.clone(),
            LlmProvider::OpenAiCompat => self.openai_compat_config.model.clone(),
            LlmProvider::Claude => self.claude_config.model.clone(),
        }
    }

    pub fn set_active_chat_model(&mut self, model: String) {
        match self.active_llm {
            LlmProvider::Gemini => self.gemini_config.chat_model = model,
            LlmProvider::OpenAiCompat => self.openai_compat_config.model = model,
            LlmProvider::Claude => self.claude_config.model = model,
        }
    }

    pub fn update_active_model_slot(&mut self, index: usize, model: String) {
        match self.active_llm {
            LlmProvider::Gemini => {
                while self.gemini_config.model_slots.len() <= index {
                    self.gemini_config.model_slots.push("".to_string());
                }
                self.gemini_config.model_slots[index] = model;
            }
            LlmProvider::OpenAiCompat => {
                while self.openai_compat_config.model_slots.len() <= index {
                    self.openai_compat_config.model_slots.push("".to_string());
                }
                self.openai_compat_config.model_slots[index] = model;
            }
            LlmProvider::Claude => {
                while self.claude_config.model_slots.len() <= index {
                    self.claude_config.model_slots.push("".to_string());
                }
                self.claude_config.model_slots[index] = model;
            }
        }
    }

    /// Get the currently active Composio profile (matched by stable ID)
    pub fn get_active_profile(&self) -> Option<&ComposioProfile> {
        if let Some(active_id) = &self.active_composio_profile {
            self.composio_profiles.iter().find(|p| &p.id == active_id)
        } else {
            None
        }
    }

    /// Get a mutable reference to the active Composio profile (matched by stable ID)
    pub fn get_active_profile_mut(&mut self) -> Option<&mut ComposioProfile> {
        if let Some(active_id) = &self.active_composio_profile {
            let id = active_id.clone();
            self.composio_profiles.iter_mut().find(|p| p.id == id)
        } else {
            self.composio_profiles.first_mut()
        }
    }

    /// Resolve a profile ID to its human-readable name.
    /// Returns None if the ID doesn't match any profile.
    pub fn profile_name_for_id(&self, id: &str) -> Option<&str> {
        self.composio_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.as_str())
    }

    /// Resolve a session's composio_profile (ID) to its display name,
    /// with fallback to the global active profile name.
    pub fn resolve_session_profile_display_name(
        &self,
        session_profile_id: Option<&str>,
    ) -> Option<String> {
        session_profile_id
            .and_then(|id| self.profile_name_for_id(id))
            .map(|n| n.to_string())
            .or_else(|| self.get_active_profile().map(|p| p.name.clone()))
    }

    /// Add a new Composio profile
    pub fn add_profile(&mut self, profile: ComposioProfile) {
        // Ensure unique name
        let existing_names: Vec<_> = self.composio_profiles.iter().map(|p| &p.name).collect();
        let mut name = profile.name.clone();
        let mut counter = 1;
        while existing_names.contains(&&name) {
            name = format!("{} ({})", profile.name, counter);
            counter += 1;
        }

        let mut new_profile = profile;
        new_profile.name = name;

        // Auto-generate user_id if not set:
        // - Inherit from first existing profile if available
        // - Otherwise generate a new lowercase UUID
        if new_profile.user_id.is_none() {
            new_profile.user_id = self
                .composio_profiles
                .first()
                .and_then(|p| p.user_id.clone())
                .or_else(|| Some(uuid::Uuid::new_v4().to_string().to_lowercase()));
        }

        // Ensure ID is set
        if new_profile.id.is_empty() {
            new_profile.id = Uuid::new_v4().to_string();
        }

        // Capture the ID before moving ownership
        let new_id = new_profile.id.clone();
        self.composio_profiles.push(new_profile);

        // Always activate newly added profiles so the UI switches to them
        self.active_composio_profile = Some(new_id);
    }

    /// Remove a Composio profile by its stable ID.
    pub fn remove_profile(&mut self, id: &str) {
        self.composio_profiles.retain(|p| p.id != id);

        // If we removed the active profile, reset to first available
        if self.active_composio_profile.as_deref() == Some(id) {
            self.active_composio_profile = self.composio_profiles.first().map(|p| p.id.clone());
        }
    }

    /// Migrate legacy single Composio settings to a profile
    #[allow(dead_code)]
    pub fn migrate_legacy_composio_settings(&mut self) {
        // Only migrate if we have legacy settings and no profiles yet
        let has_legacy = self.composio_base_url.is_some()
            || self.composio_entity_id.is_some()
            || self.composio_user_id.is_some()
            || self.composio_api_key.is_some();

        if has_legacy && self.composio_profiles.is_empty() {
            tracing::info!("Migrating legacy Composio settings to profile...");
            let profile = ComposioProfile {
                id: Uuid::new_v4().to_string(),
                name: "Default".to_string(),
                base_url: self.composio_base_url.take(),
                entity_id: self.composio_entity_id.take(),
                user_id: self.composio_user_id.take(),
                api_key: self.composio_api_key.take(),
                toolkit_configs: Vec::new(),
                color: default_profile_color(),
                chrome_profile_directory: None,
            };
            let profile_id = profile.id.clone();
            self.add_profile(profile);
            self.active_composio_profile = Some(profile_id);
        }
    }

    /// Migrate `active_composio_profile` from name-based to ID-based.
    /// Existing settings may store the profile *name* instead of the stable *id*.
    /// This checks: if the value doesn't match any profile ID but does match a name, swap it.
    pub fn migrate_active_profile_name_to_id(&mut self) {
        if let Some(ref current_value) = self.active_composio_profile {
            // Already matches an ID — nothing to do
            if self
                .composio_profiles
                .iter()
                .any(|p| &p.id == current_value)
            {
                return;
            }
            // Try matching by name instead
            if let Some(profile) = self
                .composio_profiles
                .iter()
                .find(|p| &p.name == current_value)
            {
                let id = profile.id.clone();
                tracing::info!(
                    "Migrating active_composio_profile from name '{}' to id '{}'",
                    current_value,
                    id
                );
                self.active_composio_profile = Some(id);
            }
        }
    }

    /// Ensure chat_model is synced to a configured slot.
    /// If the current chat_model doesn't match any non-empty slot, set it to slot 1's model.
    pub fn sync_chat_model_to_slots(&mut self) {
        let slots = self.active_model_slots();
        let non_empty_slots: Vec<&String> = slots.iter().filter(|s| !s.is_empty()).collect();
        if non_empty_slots.is_empty() {
            return;
        }

        match self.active_llm {
            LlmProvider::Gemini => {
                let in_a_slot = non_empty_slots
                    .iter()
                    .any(|s| **s == self.gemini_config.chat_model);
                if !in_a_slot {
                    self.gemini_config.chat_model = non_empty_slots[0].clone();
                }
            }
            LlmProvider::OpenAiCompat => {
                let in_a_slot = non_empty_slots
                    .iter()
                    .any(|s| **s == self.openai_compat_config.model);
                if !in_a_slot {
                    self.openai_compat_config.model = non_empty_slots[0].clone();
                }
            }
            LlmProvider::Claude => {
                let in_a_slot = non_empty_slots
                    .iter()
                    .any(|s| **s == self.claude_config.model);
                if !in_a_slot {
                    self.claude_config.model = non_empty_slots[0].clone();
                }
            }
        }
    }

    /// Ensure all profiles have a valid UUID.
    /// If an ID is missing or empty, generate a new one.
    pub fn ensure_profile_ids(&mut self) {
        for profile in &mut self.composio_profiles {
            if profile.id.is_empty() {
                profile.id = Uuid::new_v4().to_string();
                tracing::info!(
                    "Migrated profile '{}' with new ID: {}",
                    profile.name,
                    profile.id
                );
            }
        }
        // Save is handled by the caller (load)
    }
}
fn default_max_tool_output_length() -> usize {
    2000
}

fn default_max_active_tool_output_length() -> usize {
    500_000 // ~125k tokens, well within 1M limit but high enough for most data
}

fn default_max_summary_chars() -> usize {
    4000 // ~1000 tokens
}

fn default_max_entity_count() -> usize {
    50
}

fn default_tool_result_budget_ratio() -> f64 {
    0.30
}

/// Default chars/token ratio. English prose averages ~4 chars per token;
/// CJK/code-heavy content is closer to ~2. Exposed in settings for tuning.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 4.0;

/// Fully-resolved context tuning values (no Options — all guaranteed).
/// Created by `Settings::effective_context_tuning()` which cascades:
/// Provider preset → Global settings → Compiled defaults.
pub struct ResolvedContextTuning {
    pub chat_history_length: usize,
    pub max_tool_output_length: usize,
    pub max_active_tool_output_length: usize,
    pub max_summary_chars: usize,
    pub max_entity_count: usize,
    /// When true, convert tool results from JSON to compact markdown.
    /// Reduces token usage for models with small context windows.
    pub compact_tool_results: bool,
    pub tool_result_budget_ratio: f64,
    pub active_result_budget_ratio: f64,
    pub context_safety_margin: f64,
    pub system_prompt_budget_ratio: f64,
    /// Characters per token ratio for context budget calculations.
    /// English prose ≈ 4.0, CJK/code-heavy ≈ 2.0.
    pub chars_per_token: f64,
}

fn default_true() -> bool {
    true
}

#[derive(Clone)]
pub struct SettingsManager {
    settings_path: PathBuf,
}

impl SettingsManager {
    pub fn new(settings_path: PathBuf) -> Self {
        Self { settings_path }
    }

    pub fn load(&self) -> Settings {
        if !self.settings_path.exists() {
            let default_settings = Settings::default();
            // Attempt to save the default settings on first load
            if self.save(&default_settings).is_err() {
                tracing::error!("Failed to save default settings on initial load.");
            }
            return default_settings;
        }

        let content = match fs::read_to_string(&self.settings_path) {
            Ok(c) => c,
            Err(_) => return Settings::default(),
        };

        // First, try to deserialize directly. If it works, we're done.
        if let Ok(mut settings) = serde_json::from_str::<Settings>(&content) {
            settings.migrate_legacy_composio_settings();
            settings.ensure_profile_ids();
            // Happy-path migration: JSON deserialized cleanly but active_composio_profile
            // may still hold a legacy profile *name*. Convert it to a stable *id*.
            // This also runs in the fallback path below (L894) for the same reason — both
            // paths must independently guarantee the value is ID-based before the app boots.
            settings.migrate_active_profile_name_to_id();
            settings.sync_chat_model_to_slots();
            return settings;
        }

        // If direct deserialization fails, try a field-by-field migration.
        tracing::warn!("Failed to deserialize settings directly, attempting migration...");
        let mut settings = Settings::default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(active_llm_val) = value.get("active_llm") {
                if let Ok(active_llm) = serde_json::from_value(active_llm_val.clone()) {
                    settings.active_llm = active_llm;
                }
            }
            if let Some(openai_val) = value.get("openai_compat_config") {
                if let Ok(config) = serde_json::from_value(openai_val.clone()) {
                    settings.openai_compat_config = config;
                }
            }
            if let Some(claude_val) = value.get("claude_config") {
                if let Ok(config) = serde_json::from_value(claude_val.clone()) {
                    settings.claude_config = config;
                }
            }
            if let Some(gemini_config_val) = value.get("gemini_config") {
                if let Ok(gemini_config) = serde_json::from_value(gemini_config_val.clone()) {
                    settings.gemini_config = gemini_config;
                }
            }
            // For backwards compatibility, migrate old fields if gemini_config doesn't exist
            else {
                if let Some(chat_model) = value.get("chat_model").and_then(|v| v.as_str()) {
                    settings.gemini_config.chat_model = chat_model.to_string();
                }
                if let Some(summary_model) = value.get("summary_model").and_then(|v| v.as_str()) {
                    settings.gemini_config.summary_model = summary_model.to_string();
                }
                if let Some(slots) = value.get("model_slots").and_then(|v| v.as_array()) {
                    let slots_vec: Vec<String> = slots
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    if !slots_vec.is_empty() {
                        settings.gemini_config.model_slots = slots_vec;
                    }
                }
            }
            if let Some(persona) = value.get("persona").and_then(|v| v.as_str()) {
                settings.persona = persona.to_string();
            }
            if let Some(user_name) = value.get("user_name").and_then(|v| v.as_str()) {
                settings.user_name = Some(user_name.to_string());
            }
            if let Some(project_folder) = value.get("project_folder").and_then(|v| v.as_str()) {
                settings.project_folder = Some(project_folder.to_string());
            }
            if let Some(history_len) = value.get("chat_history_length").and_then(|v| v.as_u64()) {
                settings.chat_history_length = history_len as usize;
            }
            if let Some(show_tray) = value.get("show_tray_icon").and_then(|v| v.as_bool()) {
                settings.show_tray_icon = show_tray;
            }
            if let Some(hotkey) = value.get("global_hotkey").and_then(|v| v.as_str()) {
                settings.hotkeys.toggle_tray = hotkey.to_string();
            }
            // Note: Complex nested structs like permission_settings are harder to migrate
            // field-by-field and will fall back to default if they fail to parse.
            if let Some(perms) = value.get("permission_settings") {
                if let Ok(mut permission_settings) =
                    serde_json::from_value::<PermissionSettings>(perms.clone())
                {
                    // Manual migration for mcp_server_permissions if it was somehow skipped by serde default
                    if permission_settings.mcp_server_permissions.is_empty() {
                        if let Some(mcp_perms) = perms.get("mcp_server_permissions") {
                            if let Ok(map) = serde_json::from_value(mcp_perms.clone()) {
                                permission_settings.mcp_server_permissions = map;
                            }
                        }
                    }
                    settings.permission_settings = permission_settings;
                }
            }
            if let Some(source) = value.get("preferred_mcp_source") {
                if let Ok(source) = serde_json::from_value(source.clone()) {
                    settings.preferred_mcp_source = source;
                }
            }
            if let Some(url) = value.get("composio_base_url").and_then(|v| v.as_str()) {
                settings.composio_base_url = Some(url.to_string());
            }
            if let Some(entity_id) = value.get("composio_entity_id").and_then(|v| v.as_str()) {
                settings.composio_entity_id = Some(entity_id.to_string());
            }
            if let Some(len) = value.get("max_tool_output_length").and_then(|v| v.as_u64()) {
                settings.max_tool_output_length = len as usize;
            }
            if let Some(len) = value
                .get("max_active_tool_output_length")
                .and_then(|v| v.as_u64())
            {
                settings.max_active_tool_output_length = len as usize;
            }
            if let Some(uid) = value.get("composio_user_id").and_then(|v| v.as_str()) {
                settings.composio_user_id = Some(uid.to_string());
            }

            // AUTO-MIGRATION: Deprecated Model Check (Jan 2026)
            // If the summary model is still the old default "gemini-1.5-flash-latest",
            // automatically upgrade it to "gemini-2.5-flash".
            if settings.gemini_config.summary_model == "gemini-1.5-flash-latest" {
                tracing::info!("Migrating deprecated summary_model to gemini-2.5-flash");
                settings.gemini_config.summary_model = "gemini-2.5-flash".to_string();
            }
        }

        // Migration: Ensure all composio profiles have an ID
        // This backfills UUIDs for existing profiles that were created before the 'id' field existed.
        settings.ensure_profile_ids();
        // Fallback-path migration: field-by-field reconstruction may still leave
        // active_composio_profile as a legacy name. Defensively convert to ID here too.
        // (Mirrors the happy-path call at L793.)
        settings.migrate_active_profile_name_to_id();

        // After migrating, save the repaired settings file for the next run.
        if self.save(&settings).is_err() {
            tracing::error!("Failed to save migrated settings.");
        }

        settings.sync_chat_model_to_slots();
        settings
    }

    pub fn save(&self, settings: &Settings) -> Result<(), std::io::Error> {
        let content = serde_json::to_string_pretty(settings)?;
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Safety net: backup before overwrite to prevent data loss from deserialization regressions
        if self.settings_path.exists() {
            let backup_path = self.settings_path.with_extension("json.bak");
            if let Err(e) = fs::copy(&self.settings_path, &backup_path) {
                tracing::warn!("Failed to create settings backup: {}", e);
            }
        }
        fs::write(&self.settings_path, content)
    }

    pub fn save_async(
        &self,
        settings: Settings,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        let manager = self.clone();
        crate::async_persist::persist_async(
            move || manager.save(&settings),
            "settings",
            error_signal,
        );
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum SettingsTab {
    General,
    Mcp,
    Behavior,
    Data,
    Permissions,
    Hotkeys,
    Credentials,
    Skills,
    About,
    ImageGen,
}

impl Default for SettingsTab {
    fn default() -> Self {
        Self::General
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UiState {
    pub settings_panel_width: f64,
    /// Default state for tool call Arguments section (expanded or collapsed)
    #[serde(default = "default_true", alias = "show_tool_arguments")]
    pub default_tool_arguments_open: bool,
    /// Default state for tool call Response section (expanded or collapsed)
    #[serde(default, alias = "show_tool_response")]
    pub default_tool_response_open: bool,
    /// Default state for tool call Thinking Process section (expanded or collapsed)
    #[serde(default, alias = "show_tool_thought")]
    pub default_tool_thought_open: bool,
    /// Default state for skill call Arguments section (expanded or collapsed)
    #[serde(default = "default_true")]
    pub default_skill_arguments_open: bool,
    /// Default state for skill call Response section (expanded or collapsed)
    #[serde(default = "default_true")]
    pub default_skill_response_open: bool,
    /// Default state for skill Instructions section (expanded or collapsed)
    #[serde(default)]
    pub default_skill_instructions_open: bool,
    /// MCP servers that are unloaded (tools hidden from AI)
    #[serde(default)]
    pub unloaded_mcp_servers: Vec<String>,
    /// MCP servers in on-demand mode (tools discoverable via MCP_LOAD_SERVER_TOOLS meta-tool)
    #[serde(default)]
    pub on_demand_mcp_servers: Vec<String>,
    /// Last active tab in Settings Panel
    #[serde(default)]
    pub active_settings_tab: SettingsTab,
    /// Whether LLM config section is collapsed
    #[serde(default)]
    pub llm_config_collapsed: bool,
    /// Whether MCP instructions are collapsed
    #[serde(default)]
    pub mcp_instructions_collapsed: bool,
    /// Whether the Composio toolkit config panel is expanded
    #[serde(default)]
    pub composio_toolkit_expanded: bool,
    /// Whether to show the session cost icon in the chat bar
    #[serde(default = "default_true")]
    pub show_session_cost_icon: bool,
    /// What token/cost info to display: "all", "tokens", "cost", "none"
    #[serde(default = "default_token_display_mode")]
    pub token_display_mode: String,

    // Feature Toggles (Chat Bar Icons)
    #[serde(default = "default_true")]
    pub show_history_icon: bool,
    #[serde(default = "default_true")]
    pub show_mcp_icon: bool,
    #[serde(default = "default_true")]
    pub show_profile_selector: bool,
    #[serde(default = "default_true")]
    pub show_attachments_icon: bool,
    #[serde(default = "default_true")]
    pub show_model_selector: bool,
    /// Whether model quick-switch slots section is expanded in settings
    #[serde(default = "default_true")]
    pub show_model_slots: bool,
    /// Slug of the toolkit currently selected for BYOA credential setup
    #[serde(default)]
    pub selected_byoa_slug: Option<String>,
    /// Currently open session tabs (list of session IDs)
    #[serde(default)]
    pub open_tabs: Vec<String>,
    /// Currently focused tab index (0-based)
    #[serde(default)]
    pub active_tab_index: usize,
}

fn default_token_display_mode() -> String {
    "all".to_string()
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            settings_panel_width: 420.0, // Comfortable width for 1440px window
            default_tool_arguments_open: true,
            default_tool_response_open: false,
            default_tool_thought_open: false,
            default_skill_arguments_open: true,
            default_skill_response_open: true,
            default_skill_instructions_open: true,
            unloaded_mcp_servers: Vec::new(),
            on_demand_mcp_servers: Vec::new(),
            active_settings_tab: SettingsTab::default(),
            llm_config_collapsed: false,
            open_tabs: Vec::new(),
            active_tab_index: 0,
            mcp_instructions_collapsed: false,
            composio_toolkit_expanded: false,
            selected_byoa_slug: None,
            show_session_cost_icon: true,
            token_display_mode: "all".to_string(),
            show_history_icon: true,
            show_mcp_icon: true,
            show_profile_selector: true,
            show_attachments_icon: true,
            show_model_selector: true,
            show_model_slots: true,
        }
    }
}

#[derive(Clone)]
pub struct UiStateManager {
    state_path: PathBuf,
}

impl UiStateManager {
    pub fn new(state_path: PathBuf) -> Self {
        Self { state_path }
    }

    pub fn load(&self) -> UiState {
        if !self.state_path.exists() {
            let default_state = UiState::default();
            if self.save(&default_state).is_err() {
                tracing::error!("Failed to save default UI state on initial load.");
            }
            return default_state;
        }

        let content = match fs::read_to_string(&self.state_path) {
            Ok(c) => c,
            Err(_) => return UiState::default(),
        };

        serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::error!("Failed to deserialize UI state, using default: {}", e);
            UiState::default()
        })
    }

    pub fn save(&self, state: &UiState) -> Result<(), std::io::Error> {
        let content = serde_json::to_string_pretty(state)?;
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.state_path, content)
    }

    pub fn save_async(
        &self,
        state: UiState,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        let manager = self.clone();
        crate::async_persist::persist_async(move || manager.save(&state), "UI state", error_signal);
    }
}

// ============================================================================
// CHROME PROFILE DISCOVERY
// ============================================================================

/// Information about a discovered Chrome browser profile
#[derive(Clone, Debug)]
pub struct ChromeProfileInfo {
    /// Chrome's internal directory name (e.g., "Default", "Profile 1")
    /// This is the value passed to --profile-directory
    pub dir_name: String,
    /// User-set display name (e.g., "pugetsystems.com")
    pub display_name: String,
    /// Google account email (e.g., "dmoore@pugetsystems.com")
    pub email: Option<String>,
}

/// Discover installed Chrome profiles by reading the Local State file.
/// Returns an empty vec if Chrome is not installed or Local State is unreadable.
///
/// TODO(dustmoo): This is called synchronously in the settings panel on every render
/// when the Chrome Profile dropdown is open — a pattern violation (filesystem I/O on the
/// main thread). Cache the results in a signal or lazy_static and invalidate on
/// settings panel open/close. Low-priority: Chrome's Local State is small (~10KB).
pub fn discover_chrome_profiles() -> Vec<ChromeProfileInfo> {
    let local_state_path = {
        #[cfg(target_os = "macos")]
        {
            dirs::home_dir()
                .map(|h| h.join("Library/Application Support/Google/Chrome/Local State"))
        }
        #[cfg(target_os = "windows")]
        {
            dirs::data_local_dir().map(|d| d.join("Google/Chrome/User Data/Local State"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            dirs::config_dir().map(|c| c.join("google-chrome/Local State"))
        }
    };

    let Some(path) = local_state_path else {
        tracing::debug!("Could not determine Chrome Local State path");
        return Vec::new();
    };

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("Could not read Chrome Local State: {}", e);
            return Vec::new();
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("Could not parse Chrome Local State: {}", e);
            return Vec::new();
        }
    };

    let Some(info_cache) = json
        .get("profile")
        .and_then(|p| p.get("info_cache"))
        .and_then(|ic| ic.as_object())
    else {
        tracing::debug!("No profile.info_cache in Chrome Local State");
        return Vec::new();
    };

    let mut profiles: Vec<ChromeProfileInfo> = info_cache
        .iter()
        .map(|(dir_name, info)| {
            let display_name = info
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(dir_name)
                .to_string();
            let email = info
                .get("user_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            ChromeProfileInfo {
                dir_name: dir_name.clone(),
                display_name,
                email,
            }
        })
        .collect();

    // Sort: Default first, then alphabetically by display name
    profiles.sort_by(|a, b| {
        if a.dir_name == "Default" {
            std::cmp::Ordering::Less
        } else if b.dir_name == "Default" {
            std::cmp::Ordering::Greater
        } else {
            a.display_name.cmp(&b.display_name)
        }
    });

    tracing::debug!("Discovered {} Chrome profiles", profiles.len());
    profiles
}
