use crate::context::permissions::{PermissionSettings, ToolCategory};
pub use crate::llm::{ClaudeConfig, GeminiConfig, OpenAiCompatConfig};
pub use crate::llm::config::ProviderInstanceConfig;
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

    /// All provider variants for UI iteration.
    pub fn all_variants() -> &'static [LlmProvider] {
        &[Self::Gemini, Self::OpenAiCompat, Self::Claude]
    }

    /// Single-letter badge for the chat-bar provider picker.
    pub fn initial(&self) -> &'static str {
        match self {
            Self::Gemini => "G",
            Self::OpenAiCompat => "O",
            Self::Claude => "C",
        }
    }

    /// Tailwind background class for the provider badge (mirrors profile colors).
    pub fn color_class(&self) -> &'static str {
        match self {
            Self::Gemini => "bg-blue-600",
            Self::OpenAiCompat => "bg-emerald-600",
            Self::Claude => "bg-orange-600",
        }
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
    #[serde(default = "default_toggle_provider")]
    pub toggle_provider: String,
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
            toggle_provider: default_toggle_provider(),
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
fn default_toggle_provider() -> String {
    "CmdOrCtrl+Shift+L".to_string()
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
    /// DEPRECATED: global default provider kind. Kept for migration and for
    /// resolving legacy sessions that only pinned a provider kind. New code
    /// should use `active_connector_id` / `active_connector()`.
    pub active_llm: LlmProvider,
    /// DEPRECATED: legacy singleton per-provider configs. Read once by
    /// `migrate_legacy_llm_config` to synthesize `llm_connectors`; kept
    /// serialized for one release of downgrade safety.
    pub gemini_config: GeminiConfig,
    #[serde(default)]
    pub openai_compat_config: OpenAiCompatConfig,
    #[serde(default)]
    pub claude_config: ClaudeConfig,
    /// Named LLM connector instances ("flavors"). Multiple instances per
    /// provider kind are allowed, capped at MAX_LLM_CONNECTORS total.
    #[serde(default)]
    pub llm_connectors: Vec<ProviderInstance>,
    /// The globally active connector (default for new sessions). Sessions can
    /// pin their own via `Session.llm_connector_id`.
    #[serde(default)]
    pub active_connector_id: Option<String>,
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
    #[serde(default = "default_true", alias = "confirm_on_message_delete")]
    pub confirm_on_message_edit: bool,
    #[serde(default = "default_true")]
    pub confirm_forget_memory: bool,
    /// When a timer fires, bring the Hobbes window to the front / focus it.
    /// Off by default — stealing focus is disruptive; users opt in.
    #[serde(default)]
    pub timer_focus_window: bool,
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
    /// When true, convert tool results from JSON to TOON (compact tabular notation)
    /// before sending to the AI. Reduces token usage by 30-50% for structured tool output.
    /// Can be overridden per-provider in Context Tuning config.
    #[serde(default = "default_true")]
    pub compact_tool_results: bool,
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
    /// True if this toolkit requires no authentication (e.g. hackernews).
    /// No-auth toolkits never have a connected account, so this locally-known
    /// flag is what marks them as connected in the UI.
    #[serde(default)]
    pub no_auth: bool,
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

/// Maximum number of LLM connector instances across all provider kinds.
/// Matches the 10-model-slot / digit-hotkey idiom used elsewhere.
pub const MAX_LLM_CONNECTORS: usize = 10;

/// A named LLM connector instance ("flavor") — one configured endpoint/key
/// combination of a given provider kind. Mirrors the ComposioProfile pattern.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProviderInstance {
    #[serde(default = "default_uuid")]
    pub id: String,
    /// Unique display name shown in the chat-bar selector and settings list.
    pub name: String,
    pub config: ProviderInstanceConfig,
}

impl ProviderInstance {
    /// The provider kind of this connector.
    pub fn provider(&self) -> LlmProvider {
        self.config.provider()
    }

    /// Single-letter badge for pickers, derived from the connector's display
    /// name so same-kind instances are distinguishable (falls back to the
    /// provider kind's initial for blank names).
    pub fn initial(&self) -> String {
        self.name
            .trim()
            .chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| self.provider().initial().to_string())
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

    /// Remove a toolkit's config entry (used when a toolkit is fully removed
    /// from the Composio server). Mirrors `Settings::remove_profile`.
    pub fn remove_toolkit_config(&mut self, slug: &str) {
        self.toolkit_configs
            .retain(|c| !c.slug.eq_ignore_ascii_case(slug));
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
            llm_connectors: Vec::new(),
            active_connector_id: None,
            persona: "You are Hobbes. Be a direct, clear, and radically candid partner. Function as an exocortex, matching the user's communication style. Your default tone is that of a professional friend.".to_string(),
            user_name: None,
            force_tool_use_instruction: Some("You must always use the provided tools to answer the user's request, even if you think you know the answer. Do not answer from your own knowledge base when tools are available. When using the fetch tool, you MUST provide markdown links as sources. When you use tools you MUST use the information from the tool, if you don't have it, look up fresh information instead of guessing.".to_string()),
            project_folder: None,
            chat_history_length: 75,
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
            confirm_on_message_edit: true,
            confirm_forget_memory: true,
            timer_focus_window: false,
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
            compact_tool_results: true,
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
        self.model_slots_for(self.active_llm)
    }

    /// Quick-switch model slots for a specific provider.
    pub fn model_slots_for(&self, provider: LlmProvider) -> Vec<String> {
        match provider {
            LlmProvider::Gemini => self.gemini_config.model_slots.clone(),
            LlmProvider::OpenAiCompat => self.openai_compat_config.model_slots.clone(),
            LlmProvider::Claude => self.claude_config.model_slots.clone(),
        }
    }

    /// The provider kind a session should use, resolved through its connector.
    pub fn provider_for_session(&self, session: &crate::session::Session) -> LlmProvider {
        self.connector_for_session(session)
            .map(|c| c.provider())
            .or(session.llm_provider)
            .unwrap_or(self.active_llm)
    }

    /// The chat model a session should use: its override (when set), else the
    /// configured model of the session's connector. The override is ignored
    /// when connector resolution fell back to a different provider kind than
    /// the one the model was pinned for (e.g. the pinned connector was
    /// deleted) — sending another provider's model string would fail every
    /// request in the session.
    pub fn chat_model_for_session(&self, session: &crate::session::Session) -> String {
        let connector = self.connector_for_session(session);
        let pin_matches_connector = match (session.llm_provider, connector.map(|c| c.provider())) {
            (Some(pinned), Some(resolved)) => pinned == resolved,
            _ => true,
        };
        if pin_matches_connector {
            if let Some(model) = session.chat_model.clone().filter(|m| !m.is_empty()) {
                return model;
            }
        }
        connector
            .map(|c| c.config.chat_model())
            .unwrap_or_else(|| self.chat_model_for(self.active_llm))
    }

    /// Resolve the effective context window for a specific provider + model.
    /// Returns `None` only for unconfigured providers with no known context limit.
    pub fn resolve_context_window_for(&self, provider: LlmProvider, model: &str) -> Option<usize> {
        match provider {
            LlmProvider::OpenAiCompat => {
                // Resolution order: explicit user override → known-model name table
                // → None. The result is then clamped by any window learned at
                // runtime from a "context length exceeded" error on this endpoint,
                // so a too-optimistic estimate self-corrects after the first
                // rejection. The learned cache is scoped by endpoint URL because
                // limits are per-server.
                let base = self
                    .openai_compat_config
                    .max_context_tokens
                    .or_else(|| crate::llm::openai_models::known_context_window(model));
                crate::llm::context_cache::clamp_to_learned(
                    &self.openai_compat_config.endpoint,
                    model,
                    base,
                )
            }
            // NOTE: claude_config.max_tokens is the OUTPUT token cap (required by the
            // Messages API), not the context window — budget by the model's window.
            LlmProvider::Claude => {
                let claude_model = crate::llm::claude_models::ClaudeModel::from_slug(model);
                crate::llm::context_cache::clamp_to_learned(
                    "claude",
                    model,
                    Some(claude_model.context_window_tokens()),
                )
            }
            LlmProvider::Gemini => {
                let gemini_model = crate::llm::gemini::GeminiModel::from_slug(model);
                crate::llm::context_cache::clamp_to_learned(
                    "gemini",
                    model,
                    Some(gemini_model.context_window_tokens()),
                )
            }
        }
    }

    /// Resolve the effective context tuning for a specific provider.
    /// Provider preset overrides → Global settings → Compiled defaults.
    pub fn effective_context_tuning_for(&self, provider: LlmProvider) -> ResolvedContextTuning {
        use crate::llm::config::ContextTuningPreset;
        let preset: &ContextTuningPreset = match provider {
            LlmProvider::Gemini => &self.gemini_config.context_tuning,
            LlmProvider::OpenAiCompat => &self.openai_compat_config.context_tuning,
            LlmProvider::Claude => &self.claude_config.context_tuning,
        };
        self.resolve_context_tuning(preset)
    }

    /// Cascade a context-tuning preset over global settings and compiled defaults.
    fn resolve_context_tuning(
        &self,
        preset: &crate::llm::config::ContextTuningPreset,
    ) -> ResolvedContextTuning {
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
            compact_tool_results: preset.compact_tool_results.unwrap_or(self.compact_tool_results),
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
        self.chat_model_for(self.active_llm)
    }

    /// The configured chat model for a specific provider.
    pub fn chat_model_for(&self, provider: LlmProvider) -> String {
        match provider {
            LlmProvider::Gemini => self.gemini_config.chat_model.clone(),
            LlmProvider::OpenAiCompat => self.openai_compat_config.model.clone(),
            LlmProvider::Claude => self.claude_config.model.clone(),
        }
    }

    /// Set the configured chat model for a specific provider.
    /// Part of the provider-aware Settings API; retained for completeness even
    /// when no current call site uses it.
    #[allow(dead_code)]
    pub fn set_chat_model_for(&mut self, provider: LlmProvider, model: String) {
        match provider {
            LlmProvider::Gemini => self.gemini_config.chat_model = model,
            LlmProvider::OpenAiCompat => self.openai_compat_config.model = model,
            LlmProvider::Claude => self.claude_config.model = model,
        }
    }

    /// Set a quick-switch model slot on a specific provider kind's editing
    /// mirror (the settings panel flushes mirrors into the selected connector).
    pub fn update_model_slot_for(&mut self, provider: LlmProvider, index: usize, model: String) {
        let slots = match provider {
            LlmProvider::Gemini => &mut self.gemini_config.model_slots,
            LlmProvider::OpenAiCompat => &mut self.openai_compat_config.model_slots,
            LlmProvider::Claude => &mut self.claude_config.model_slots,
        };
        while slots.len() <= index {
            slots.push(String::new());
        }
        slots[index] = model;
    }

    // ========================================================================
    // LLM connector instances ("flavors")
    // ========================================================================

    /// Look up a connector by its stable ID.
    pub fn connector_by_id(&self, id: &str) -> Option<&ProviderInstance> {
        self.llm_connectors.iter().find(|c| c.id == id)
    }

    /// Mutable lookup of a connector by its stable ID.
    pub fn connector_by_id_mut(&mut self, id: &str) -> Option<&mut ProviderInstance> {
        self.llm_connectors.iter_mut().find(|c| c.id == id)
    }

    /// The globally active connector: `active_connector_id` when it resolves,
    /// else the first connector (stale-id fallback, mirrors get_active_profile_mut).
    pub fn active_connector(&self) -> Option<&ProviderInstance> {
        self.active_connector_id
            .as_deref()
            .and_then(|id| self.connector_by_id(id))
            .or_else(|| self.llm_connectors.first())
    }

    /// Mutable access to the globally active connector (same fallback rules).
    pub fn active_connector_mut(&mut self) -> Option<&mut ProviderInstance> {
        let id = self
            .active_connector()
            .map(|c| c.id.clone())?;
        self.connector_by_id_mut(&id)
    }

    /// Add a new connector. Enforces the MAX_LLM_CONNECTORS cap and unique
    /// display names (auto-suffixed like Composio's add_profile). The new
    /// connector becomes the globally active one. Returns its ID.
    pub fn add_connector(
        &mut self,
        name: &str,
        config: ProviderInstanceConfig,
    ) -> Result<String, String> {
        if self.llm_connectors.len() >= MAX_LLM_CONNECTORS {
            return Err(format!(
                "Connector limit reached ({MAX_LLM_CONNECTORS}). Remove one to add another."
            ));
        }
        let base_name = if name.trim().is_empty() {
            config.provider().display_name().to_string()
        } else {
            name.trim().to_string()
        };
        let mut unique_name = base_name.clone();
        let mut counter = 1;
        while self.llm_connectors.iter().any(|c| c.name == unique_name) {
            counter += 1;
            unique_name = format!("{base_name} {counter}");
        }
        let instance = ProviderInstance {
            id: Uuid::new_v4().to_string(),
            name: unique_name,
            config,
        };
        let id = instance.id.clone();
        self.llm_connectors.push(instance);
        self.active_connector_id = Some(id.clone());
        self.active_llm = self
            .connector_by_id(&id)
            .map(|c| c.provider())
            .unwrap_or(self.active_llm);
        Ok(id)
    }

    /// Remove a connector by ID. Refuses to remove the last one (returns false).
    /// Reassigns the active connector to the first remaining when needed.
    pub fn remove_connector(&mut self, id: &str) -> bool {
        if self.llm_connectors.len() <= 1 {
            return false;
        }
        let before = self.llm_connectors.len();
        self.llm_connectors.retain(|c| c.id != id);
        if self.llm_connectors.len() == before {
            return false;
        }
        if self.active_connector_id.as_deref() == Some(id) {
            self.active_connector_id = self.llm_connectors.first().map(|c| c.id.clone());
            if let Some(active) = self.active_connector() {
                self.active_llm = active.provider();
            }
        }
        true
    }

    /// Rename a connector, keeping display names unique.
    pub fn rename_connector(&mut self, id: &str, name: &str) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let taken = self
            .llm_connectors
            .iter()
            .any(|c| c.id != id && c.name == trimmed);
        if taken {
            return;
        }
        if let Some(c) = self.connector_by_id_mut(id) {
            c.name = trimmed.to_string();
        }
    }

    /// The connector a session should use.
    /// Resolution: session's pinned connector ID → first connector matching the
    /// session's legacy pinned provider kind → globally active connector.
    pub fn connector_for_session(
        &self,
        session: &crate::session::Session,
    ) -> Option<&ProviderInstance> {
        if let Some(inst) = session
            .llm_connector_id
            .as_deref()
            .and_then(|id| self.connector_by_id(id))
        {
            return Some(inst);
        }
        if let Some(kind) = session.llm_provider {
            if let Some(inst) = self.llm_connectors.iter().find(|c| c.provider() == kind) {
                return Some(inst);
            }
        }
        self.active_connector()
    }

    /// Whether a connector has the minimum configuration needed to serve requests.
    pub fn is_connector_configured(&self, instance: &ProviderInstance) -> bool {
        match &instance.config {
            ProviderInstanceConfig::Gemini(c) => {
                c.api_key.is_some() || std::env::var("GEMINI_API_KEY").is_ok()
            }
            ProviderInstanceConfig::OpenAiCompat(c) => !c.endpoint.trim().is_empty(),
            ProviderInstanceConfig::Claude(c) => {
                c.api_key.is_some() || std::env::var("ANTHROPIC_API_KEY").is_ok()
            }
        }
    }

    /// Resolve the effective context window for a connector + model.
    /// Same resolution rules as `resolve_context_window_for`, but reads the
    /// instance's own config (per-instance endpoint / max_context_tokens).
    pub fn resolve_context_window_for_connector(
        &self,
        instance: &ProviderInstance,
        model: &str,
    ) -> Option<usize> {
        match &instance.config {
            ProviderInstanceConfig::OpenAiCompat(c) => {
                let base = c
                    .max_context_tokens
                    .or_else(|| crate::llm::openai_models::known_context_window(model));
                crate::llm::context_cache::clamp_to_learned(&c.endpoint, model, base)
            }
            ProviderInstanceConfig::Claude(_) => {
                let claude_model = crate::llm::claude_models::ClaudeModel::from_slug(model);
                crate::llm::context_cache::clamp_to_learned(
                    "claude",
                    model,
                    Some(claude_model.context_window_tokens()),
                )
            }
            ProviderInstanceConfig::Gemini(_) => {
                let gemini_model = crate::llm::gemini::GeminiModel::from_slug(model);
                crate::llm::context_cache::clamp_to_learned(
                    "gemini",
                    model,
                    Some(gemini_model.context_window_tokens()),
                )
            }
        }
    }

    /// Resolve the effective context tuning for a connector instance.
    /// Instance preset overrides → Global settings → Compiled defaults.
    pub fn effective_context_tuning_for_connector(
        &self,
        instance: &ProviderInstance,
    ) -> ResolvedContextTuning {
        self.resolve_context_tuning(instance.config.context_tuning())
    }

    /// Whether keychain items should be written with biometric protection:
    /// only in a sandboxed (signed) build with Biometric mode selected.
    /// Discovery-index keys are always written plain regardless — they must
    /// be readable at startup before any authentication.
    pub fn use_biometric_storage(&self) -> bool {
        is_sandboxed() && self.keychain_storage_mode == KeychainStorageMode::Biometric
    }

    /// Context tuning + resolved context window for an optional connector
    /// instance, falling back to the globally-active provider when none
    /// resolved. Shared by every "how big is the context" call site.
    pub fn tuning_and_window(
        &self,
        instance: Option<&ProviderInstance>,
        model: &str,
    ) -> (ResolvedContextTuning, Option<usize>) {
        match instance {
            Some(inst) => (
                self.effective_context_tuning_for_connector(inst),
                self.resolve_context_window_for_connector(inst, model),
            ),
            None => (
                self.effective_context_tuning_for(self.active_llm),
                self.resolve_context_window_for(self.active_llm, model),
            ),
        }
    }

    /// One-time migration: synthesize named connector instances from the legacy
    /// singleton per-provider configs. No-op once `llm_connectors` is populated.
    /// Returns true when it changed anything (caller persists).
    pub fn migrate_legacy_llm_config(&mut self) -> bool {
        if !self.llm_connectors.is_empty() {
            return false;
        }

        let mut migrated = false;
        // A provider is worth migrating when it's configured beyond defaults
        // (keys aren't hydrated at settings-load time, so we can't check them
        // here) or it is the active provider — the active one always migrates
        // so the user's selection and onboarding gating survive.
        let gemini_worth = self.gemini_config != GeminiConfig::default()
            || self.active_llm == LlmProvider::Gemini;
        let oai_worth = !self.openai_compat_config.endpoint.is_empty()
            || self.active_llm == LlmProvider::OpenAiCompat;
        let claude_worth = self.claude_config != ClaudeConfig::default()
            || self.active_llm == LlmProvider::Claude;

        let push = |connectors: &mut Vec<ProviderInstance>, config: ProviderInstanceConfig| {
            let name = config.provider().display_name().to_string();
            connectors.push(ProviderInstance {
                id: Uuid::new_v4().to_string(),
                name,
                config,
            });
        };

        if gemini_worth {
            push(
                &mut self.llm_connectors,
                ProviderInstanceConfig::Gemini(self.gemini_config.clone()),
            );
            migrated = true;
        }
        if oai_worth {
            push(
                &mut self.llm_connectors,
                ProviderInstanceConfig::OpenAiCompat(self.openai_compat_config.clone()),
            );
            migrated = true;
        }
        if claude_worth {
            push(
                &mut self.llm_connectors,
                ProviderInstanceConfig::Claude(self.claude_config.clone()),
            );
            migrated = true;
        }

        if migrated {
            self.active_connector_id = self
                .llm_connectors
                .iter()
                .find(|c| c.provider() == self.active_llm)
                .map(|c| c.id.clone());
            tracing::info!(
                "Migrated legacy LLM configs to {} connector instance(s); active: {:?}",
                self.llm_connectors.len(),
                self.active_connector_id
            );
        }
        migrated
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

    /// Seed default Claude quick-switch slots when none are assigned.
    /// Non-destructive: only fills when every slot is empty (covers a missing,
    /// empty, or all-blank persisted vec). If any slot holds a model, the
    /// configuration is left untouched. Call this AFTER `sync_chat_model_to_slots`
    /// so a user's active chat model is never reset by the seeded slots.
    pub fn seed_default_claude_slots_if_empty(&mut self) {
        let has_any = self
            .claude_config
            .model_slots
            .iter()
            .any(|s| !s.is_empty());
        if !has_any {
            self.claude_config.model_slots =
                crate::llm::config::default_claude_model_slots();
            tracing::info!("Seeded default Claude quick-switch slots (none were set).");
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
    8000
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

/// Default chars/token ratio. 3.0 balances English prose (~4 chars/token) with
/// JSON/code-heavy content (~2.5 chars/token). Intentionally conservative to
/// prevent "Prompt Too Large" errors on small context models (16K–32K).
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.0;

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

/// Atomically write `bytes` to `target` by staging a temp file in the same
/// directory, fsyncing it, then replacing the target in a single rename.
///
/// This mirrors the session-persistence pattern (P-009). Plain `fs::write`
/// truncates the file in place, which is neither atomic nor resilient on
/// Windows: a concurrent reader/writer (overlapping save tasks, an AV scanner,
/// or the search indexer holding a handle) can observe or produce a
/// half-written `settings.json`, which then fails to deserialize and silently
/// reverts to defaults on the next launch. The rename is atomic on both Unix
/// and Windows (`MoveFileExW` with replace-existing); the short retry rides out
/// transient Windows sharing violations.
fn write_atomic(
    target: &std::path::Path,
    dir: &std::path::Path,
    bytes: &[u8],
) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..3u32 {
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        match temp.persist(target) {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    "Atomic settings write attempt {} failed: {}",
                    attempt + 1,
                    e.error
                );
                last_err = Some(e.error);
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1) as u64));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("atomic settings write failed")))
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
            settings.seed_default_claude_slots_if_empty();
            if settings.migrate_legacy_llm_config() && self.save(&settings).is_err() {
                tracing::error!("Failed to persist migrated LLM connector settings.");
            }
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
        settings.migrate_legacy_llm_config();

        // After migrating, save the repaired settings file for the next run.
        if self.save(&settings).is_err() {
            tracing::error!("Failed to save migrated settings.");
        }

        settings.sync_chat_model_to_slots();
        settings.seed_default_claude_slots_if_empty();
        settings
    }

    pub fn save(&self, settings: &Settings) -> Result<(), std::io::Error> {
        let content = serde_json::to_string_pretty(settings)?;
        let parent = self.settings_path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "settings path has no parent directory",
            )
        })?;
        fs::create_dir_all(parent)?;

        // Safety net: backup before overwrite to guard against deserialization regressions.
        if self.settings_path.exists() {
            let backup_path = self.settings_path.with_extension("json.bak");
            if let Err(e) = fs::copy(&self.settings_path, &backup_path) {
                tracing::warn!("Failed to create settings backup: {}", e);
            }
        }

        write_atomic(&self.settings_path, parent, content.as_bytes())
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
    /// Whether to show the LLM provider picker in the chat bar
    #[serde(default = "default_true")]
    pub show_provider_selector: bool,
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
            show_provider_selector: true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_toolkit_config_drops_matching_slug_case_insensitively() {
        let mut profile = ComposioProfile::default();
        for slug in ["gmail", "github", "slack"] {
            profile.toolkit_configs.push(ComposioToolkitConfig {
                slug: slug.to_string(),
                display_name: slug.to_string(),
                ..Default::default()
            });
        }
        profile.remove_toolkit_config("GMAIL"); // case-insensitive
        let remaining: Vec<&str> = profile
            .toolkit_configs
            .iter()
            .map(|c| c.slug.as_str())
            .collect();
        assert_eq!(remaining, vec!["github", "slack"]);
        // Removing a non-existent slug is a no-op.
        profile.remove_toolkit_config("notthere");
        assert_eq!(profile.toolkit_configs.len(), 2);
    }

    #[test]
    fn seed_fills_empty_claude_slots() {
        let mut s = Settings::default();
        s.claude_config.model_slots = vec![]; // simulate a persisted empty vec
        s.seed_default_claude_slots_if_empty();
        assert_eq!(s.claude_config.model_slots.len(), 10);
        assert_eq!(s.claude_config.model_slots[0], "claude-opus-4-8");
    }

    #[test]
    fn seed_fills_all_blank_claude_slots() {
        let mut s = Settings::default();
        s.claude_config.model_slots = vec!["".to_string(); 10];
        s.seed_default_claude_slots_if_empty();
        assert!(!s.claude_config.model_slots[0].is_empty());
    }

    #[test]
    fn seed_preserves_user_assigned_slots() {
        let mut s = Settings::default();
        let mut slots = vec!["".to_string(); 10];
        slots[2] = "claude-haiku-4-5".to_string();
        s.claude_config.model_slots = slots.clone();
        s.seed_default_claude_slots_if_empty();
        // Any assigned slot means the configuration is left untouched.
        assert_eq!(s.claude_config.model_slots, slots);
    }

    #[test]
    fn write_atomic_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");

        write_atomic(&target, dir.path(), b"first").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");

        // Overwrite in place via atomic replace — new contents only.
        write_atomic(&target, dir.path(), b"second-and-longer").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second-and-longer");

        // No stray temp files left behind.
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1, "only settings.json should remain");
    }

    #[test]
    fn settings_manager_save_load_roundtrip_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SettingsManager::new(dir.path().join("settings.json"));

        let mut settings = Settings::default();
        settings.user_name = Some("Ada".to_string());
        mgr.save(&settings).unwrap();

        // The second save (previously the non-atomic + backup path) must persist
        // the new value rather than revert it — the Windows "doesn't save after
        // the first save" symptom.
        settings.user_name = Some("Grace".to_string());
        mgr.save(&settings).unwrap();

        assert_eq!(mgr.load().user_name.as_deref(), Some("Grace"));
    }

    // ========================================================================
    // Multi-connector ("flavors") tests
    // ========================================================================

    fn test_session() -> crate::session::Session {
        crate::session::Session {
            id: "test-session".to_string(),
            name: "test".to_string(),
            messages: vec![],
            active_context: Default::default(),
            last_updated: chrono::Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: None,
            llm_connector_id: None,
            llm_provider: None,
            chat_model: None,
            loaded_skills: HashMap::new(),
            scratchpad: String::new(),
            current_ai_turn_count: 0,
            watch_word_recovery_count: 0,
            scheduled_timers: Vec::new(),
        }
    }

    #[test]
    fn migrate_creates_instance_for_active_provider() {
        let mut s = Settings::default(); // active_llm = Gemini, all configs default
        assert!(s.migrate_legacy_llm_config());
        assert_eq!(s.llm_connectors.len(), 1);
        assert_eq!(s.llm_connectors[0].provider(), LlmProvider::Gemini);
        assert_eq!(
            s.active_connector_id.as_deref(),
            Some(s.llm_connectors[0].id.as_str())
        );
    }

    #[test]
    fn migrate_includes_configured_legacy_providers() {
        let mut s = Settings::default();
        s.openai_compat_config.endpoint = "http://localhost:11434/v1".to_string();
        s.claude_config.model = "claude-sonnet-4-6".to_string(); // non-default
        assert!(s.migrate_legacy_llm_config());
        let kinds: Vec<LlmProvider> = s.llm_connectors.iter().map(|c| c.provider()).collect();
        assert!(kinds.contains(&LlmProvider::Gemini)); // active provider always migrates
        assert!(kinds.contains(&LlmProvider::OpenAiCompat));
        assert!(kinds.contains(&LlmProvider::Claude));
        // Config payloads carried over
        let oai = s
            .llm_connectors
            .iter()
            .find(|c| c.provider() == LlmProvider::OpenAiCompat)
            .unwrap();
        match &oai.config {
            ProviderInstanceConfig::OpenAiCompat(c) => {
                assert_eq!(c.endpoint, "http://localhost:11434/v1")
            }
            _ => panic!("wrong config variant"),
        }
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut s = Settings::default();
        assert!(s.migrate_legacy_llm_config());
        let count = s.llm_connectors.len();
        let active = s.active_connector_id.clone();
        assert!(!s.migrate_legacy_llm_config());
        assert_eq!(s.llm_connectors.len(), count);
        assert_eq!(s.active_connector_id, active);
    }

    #[test]
    fn add_connector_enforces_cap_and_unique_names() {
        let mut s = Settings::default();
        for _ in 0..MAX_LLM_CONNECTORS {
            s.add_connector(
                "Local",
                ProviderInstanceConfig::OpenAiCompat(OpenAiCompatConfig::default()),
            )
            .unwrap();
        }
        assert_eq!(s.llm_connectors.len(), MAX_LLM_CONNECTORS);
        assert!(s
            .add_connector(
                "Local",
                ProviderInstanceConfig::OpenAiCompat(OpenAiCompatConfig::default()),
            )
            .is_err());
        // Unique display names auto-suffixed
        let names: std::collections::HashSet<&str> =
            s.llm_connectors.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), MAX_LLM_CONNECTORS);
        assert!(names.contains("Local"));
        assert!(names.contains("Local 2"));
    }

    #[test]
    fn add_connector_activates_new_instance() {
        let mut s = Settings::default();
        let first = s
            .add_connector("A", ProviderInstanceConfig::Gemini(GeminiConfig::default()))
            .unwrap();
        assert_eq!(s.active_connector_id.as_deref(), Some(first.as_str()));
        let second = s
            .add_connector("B", ProviderInstanceConfig::Claude(ClaudeConfig::default()))
            .unwrap();
        assert_eq!(s.active_connector_id.as_deref(), Some(second.as_str()));
        assert_eq!(s.active_llm, LlmProvider::Claude);
    }

    #[test]
    fn remove_connector_refuses_last_and_reassigns_active() {
        let mut s = Settings::default();
        let a = s
            .add_connector("A", ProviderInstanceConfig::Gemini(GeminiConfig::default()))
            .unwrap();
        assert!(!s.remove_connector(&a), "must refuse removing the last connector");

        let b = s
            .add_connector("B", ProviderInstanceConfig::Claude(ClaudeConfig::default()))
            .unwrap();
        // b is active; removing it reassigns to the first remaining (a)
        assert!(s.remove_connector(&b));
        assert_eq!(s.active_connector_id.as_deref(), Some(a.as_str()));
        assert_eq!(s.active_llm, LlmProvider::Gemini);
        // Unknown id is a no-op... but with one left it refuses anyway
        assert!(!s.remove_connector("nonexistent"));
    }

    #[test]
    fn rename_connector_keeps_names_unique() {
        let mut s = Settings::default();
        let a = s
            .add_connector("A", ProviderInstanceConfig::Gemini(GeminiConfig::default()))
            .unwrap();
        let b = s
            .add_connector("B", ProviderInstanceConfig::Gemini(GeminiConfig::default()))
            .unwrap();
        s.rename_connector(&b, "A"); // collides — ignored
        assert_eq!(s.connector_by_id(&b).unwrap().name, "B");
        s.rename_connector(&a, "Primary");
        assert_eq!(s.connector_by_id(&a).unwrap().name, "Primary");
        s.rename_connector(&b, "   "); // blank — ignored
        assert_eq!(s.connector_by_id(&b).unwrap().name, "B");
    }

    #[test]
    fn connector_for_session_fallback_chain() {
        let mut s = Settings::default();
        let gemini_id = s
            .add_connector("G", ProviderInstanceConfig::Gemini(GeminiConfig::default()))
            .unwrap();
        let claude_id = s
            .add_connector("C", ProviderInstanceConfig::Claude(ClaudeConfig::default()))
            .unwrap();
        // active is claude_id (last added)

        // 1. Pinned connector id wins
        let mut session = test_session();
        session.llm_connector_id = Some(gemini_id.clone());
        session.llm_provider = Some(LlmProvider::Gemini);
        assert_eq!(s.connector_for_session(&session).unwrap().id, gemini_id);

        // 2. Legacy session (kind only) → first connector of that kind
        let mut legacy = test_session();
        legacy.llm_provider = Some(LlmProvider::Gemini);
        assert_eq!(s.connector_for_session(&legacy).unwrap().id, gemini_id);

        // 3. Stale pinned id → kind match fallback
        let mut stale = test_session();
        stale.llm_connector_id = Some("deleted-id".to_string());
        stale.llm_provider = Some(LlmProvider::Claude);
        assert_eq!(s.connector_for_session(&stale).unwrap().id, claude_id);

        // 4. No pins at all → global active connector
        let unpinned = test_session();
        assert_eq!(s.connector_for_session(&unpinned).unwrap().id, claude_id);

        // 5. No connectors configured → None
        let empty = Settings::default();
        assert!(empty.connector_for_session(&unpinned).is_none());
    }

    #[test]
    fn chat_model_for_session_resolves_through_connector() {
        let mut s = Settings::default();
        let mut oai = OpenAiCompatConfig::default();
        oai.endpoint = "http://a:8000/v1".to_string();
        oai.model = "qwen-3.6".to_string();
        let oai_id = s
            .add_connector("Local A", ProviderInstanceConfig::OpenAiCompat(oai))
            .unwrap();

        let mut session = test_session();
        session.llm_connector_id = Some(oai_id);
        // No session model override → connector's configured model
        assert_eq!(s.chat_model_for_session(&session), "qwen-3.6");
        // Session override wins
        session.chat_model = Some("other-model".to_string());
        assert_eq!(s.chat_model_for_session(&session), "other-model");
    }

    #[test]
    fn two_instances_of_same_kind_keep_separate_configs() {
        let mut s = Settings::default();
        let mut a = OpenAiCompatConfig::default();
        a.endpoint = "http://a:8000/v1".to_string();
        a.max_context_tokens = Some(32_000);
        a.watch_words_enabled = true;
        let mut b = OpenAiCompatConfig::default();
        b.endpoint = "http://b:8000/v1".to_string();
        b.max_context_tokens = Some(8_000);
        let a_id = s
            .add_connector("A", ProviderInstanceConfig::OpenAiCompat(a))
            .unwrap();
        let b_id = s
            .add_connector("B", ProviderInstanceConfig::OpenAiCompat(b))
            .unwrap();

        let inst_a = s.connector_by_id(&a_id).unwrap();
        let inst_b = s.connector_by_id(&b_id).unwrap();
        assert_eq!(
            s.resolve_context_window_for_connector(inst_a, "some-model"),
            Some(32_000)
        );
        assert_eq!(
            s.resolve_context_window_for_connector(inst_b, "some-model"),
            Some(8_000)
        );
        match (&inst_a.config, &inst_b.config) {
            (
                ProviderInstanceConfig::OpenAiCompat(ca),
                ProviderInstanceConfig::OpenAiCompat(cb),
            ) => {
                assert!(ca.watch_words_enabled);
                assert!(!cb.watch_words_enabled);
            }
            _ => panic!("wrong config variants"),
        }
    }

    #[test]
    fn settings_json_roundtrip_preserves_connectors_but_not_keys() {
        let mut s = Settings::default();
        let mut cfg = GeminiConfig::default();
        cfg.api_key = Some("secret".to_string());
        let id = s
            .add_connector("My Gemini", ProviderInstanceConfig::Gemini(cfg))
            .unwrap();

        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("secret"),
            "api keys must never serialize into settings.json"
        );
        let restored: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.llm_connectors.len(), 1);
        let inst = restored.connector_by_id(&id).unwrap();
        assert_eq!(inst.name, "My Gemini");
        assert_eq!(inst.provider(), LlmProvider::Gemini);
        assert!(inst.config.api_key().is_none());
        assert_eq!(restored.active_connector_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn session_deserializes_without_connector_id() {
        // Old sessions.db rows have llm_provider but no llm_connector_id
        let mut session = test_session();
        session.llm_provider = Some(LlmProvider::Claude);
        let mut value = serde_json::to_value(&session).unwrap();
        value.as_object_mut().unwrap().remove("llm_connector_id");
        let restored: crate::session::Session = serde_json::from_value(value).unwrap();
        assert_eq!(restored.llm_connector_id, None);
        assert_eq!(restored.llm_provider, Some(LlmProvider::Claude));
    }
}
