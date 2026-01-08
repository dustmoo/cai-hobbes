use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use crate::context::permissions::{PermissionSettings, ToolCategory};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LlmProvider {
    Gemini,
}

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
    #[serde(default = "default_toggle_scroll_to_bottom")]
    pub toggle_scroll_to_bottom: String,
    #[serde(default = "default_toggle_focus_chat")]
    pub toggle_focus_chat: String,
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
            toggle_scroll_to_bottom: default_toggle_scroll_to_bottom(),
            toggle_focus_chat: default_toggle_focus_chat(),
        }
    }
}

fn default_toggle_settings() -> String { "CmdOrCtrl+,".to_string() }
fn default_toggle_history() -> String { "CmdOrCtrl+Shift+H".to_string() }
fn default_toggle_mcp() -> String { "CmdOrCtrl+Shift+M".to_string() }
fn default_toggle_profile() -> String { "CmdOrCtrl+Shift+P".to_string() }
fn default_toggle_attachments() -> String { "CmdOrCtrl+Shift+A".to_string() }
fn default_toggle_tray() -> String { "CmdOrCtrl+Shift+Space".to_string() }
fn default_toggle_new_chat() -> String { "CmdOrCtrl+Shift+N".to_string() }
fn default_toggle_focus_chat() -> String { "CmdOrCtrl+N".to_string() }
fn default_toggle_scroll_to_bottom() -> String { "CmdOrCtrl+Shift+ArrowDown".to_string() }

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    pub active_llm: LlmProvider,
    pub gemini_config: GeminiConfig,
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
        std::env::current_exe().ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .map(|p| p.join("embedded.provisionprofile").exists())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    { false }
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
pub const APP_ATTRIBUTION: &str = "Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families.";

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
    /// If true, all tools are loaded upfront instead of on-demand via Tool Router
    /// Default is false (on-demand via Tool Router)
    #[serde(default)]
    pub force_load: bool,
}

/// A Composio profile containing connection settings for one Composio account
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ComposioProfile {
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
}

fn default_profile_color() -> String {
    "bg-blue-600".to_string()
}

impl Default for ComposioProfile {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            base_url: None,
            entity_id: None,
            user_id: None,
            api_key: None,
            toolkit_configs: Vec::new(),
            color: default_profile_color(),
        }
    }
}

impl ComposioProfile {
    /// Get slugs of toolkits configured for force loading (upfront)
    pub fn get_force_load_toolkit_slugs(&self) -> Vec<String> {
        self.toolkit_configs
            .iter()
            .filter(|c| c.force_load)
            .map(|c| c.slug.clone())
            .collect()
    }

    /// Get slugs of toolkits configured for on-demand loading (default)
    #[allow(dead_code)]
    pub fn get_on_demand_toolkit_slugs(&self) -> Vec<String> {
        self.toolkit_configs
            .iter()
            .filter(|c| !c.force_load)
            .map(|c| c.slug.clone())
            .collect()
    }

    /// Check if a toolkit is configured for force loading
    #[allow(dead_code)]
    pub fn is_toolkit_force_load(&self, slug: &str) -> bool {
        self.toolkit_configs
            .iter()
            .find(|c| c.slug.eq_ignore_ascii_case(slug))
            .map(|c| c.force_load)
            .unwrap_or(false) // Default to on-demand if not configured
    }

    /// Update or add a toolkit configuration
    #[allow(dead_code)]
    pub fn set_toolkit_config(&mut self, config: ComposioToolkitConfig) {
        if let Some(existing) = self.toolkit_configs.iter_mut().find(|c| c.slug == config.slug) {
            *existing = config;
        } else {
            self.toolkit_configs.push(config);
        }
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
            active_llm: LlmProvider::Gemini,
            gemini_config: GeminiConfig {
                api_key: None,
                chat_model: "gemini-2.5-pro".to_string(),
                summary_model: "gemini-1.5-flash-latest".to_string(),
                thinking_enabled: false,
                thinking_level: "high".to_string(),
                thinking_budget: None,
            },
            persona: "You are Hobbes. Be a direct, clear, and radically candid partner. Function as an exocortex, matching the user's communication style. Your default tone is that of a professional friend.".to_string(),
            user_name: None,
            force_tool_use_instruction: Some("You must always use the provided tools to answer the user's request, even if you think you know the answer. Do not answer from your own knowledge base when tools are available. When using the fetch tool, you MUST provide markdown links as sources.".to_string()),
            project_folder: None,
            chat_history_length: 8,
            show_tray_icon: true,
            permission_settings: PermissionSettings {
                auto_approval_enabled: true,
                granular_permissions,
                mcp_server_permissions: HashMap::new(),
                max_ai_turns: 25,
            },
            confirm_on_delete: true,
            confirm_on_save: true,
            confirm_on_message_delete: true,
            smithery_api_key: None,
            preferred_mcp_source: McpSource::default(),
            keychain_storage_mode: KeychainStorageMode::default(),
            max_tool_output_length: default_max_tool_output_length(),
            max_active_tool_output_length: default_max_active_tool_output_length(),
            composio_profiles: Vec::new(),
            hotkeys: HotkeySettings::default(),
            active_composio_profile: None,
            // Legacy fields for migration
            composio_base_url: None,
            composio_entity_id: None,
            composio_api_key: None,
            composio_user_id: None,
        }
    }
}

impl Settings {
    /// Get the currently active Composio profile
    pub fn get_active_profile(&self) -> Option<&ComposioProfile> {
        if let Some(active_name) = &self.active_composio_profile {
            self.composio_profiles.iter().find(|p| &p.name == active_name)
        } else {
            self.composio_profiles.first()
        }
    }

    /// Get a mutable reference to the active Composio profile
    pub fn get_active_profile_mut(&mut self) -> Option<&mut ComposioProfile> {
        if let Some(active_name) = &self.active_composio_profile {
            let name = active_name.clone();
            self.composio_profiles.iter_mut().find(|p| p.name == name)
        } else {
            self.composio_profiles.first_mut()
        }
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
            new_profile.user_id = self.composio_profiles
                .first()
                .and_then(|p| p.user_id.clone())
                .or_else(|| Some(uuid::Uuid::new_v4().to_string().to_lowercase()));
        }
        
        self.composio_profiles.push(new_profile);
        
        // If this is the first profile, make it active
        if self.active_composio_profile.is_none() && self.composio_profiles.len() == 1 {
            self.active_composio_profile = Some(self.composio_profiles[0].name.clone());
        }
    }

    /// Remove a Composio profile by name
    pub fn remove_profile(&mut self, name: &str) {
        self.composio_profiles.retain(|p| p.name != name);
        
        // If we removed the active profile, reset to first available
        if self.active_composio_profile.as_deref() == Some(name) {
            self.active_composio_profile = self.composio_profiles.first().map(|p| p.name.clone());
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
                name: "Default".to_string(),
                base_url: self.composio_base_url.take(),
                entity_id: self.composio_entity_id.take(),
                user_id: self.composio_user_id.take(),
                api_key: self.composio_api_key.take(),
                toolkit_configs: Vec::new(),
                color: default_profile_color(),
            };
            self.add_profile(profile);
            self.active_composio_profile = Some("Default".to_string());
        }
    }
}
fn default_max_tool_output_length() -> usize {
    2000
}

fn default_max_active_tool_output_length() -> usize {
    500_000 // ~125k tokens, well within 1M limit but high enough for most data
}

fn default_true() -> bool {
    true
}

fn default_thinking_level() -> String {
    "high".to_string()
}

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
            return settings;
        }

        // If direct deserialization fails, try a field-by-field migration.
        tracing::warn!("Failed to deserialize settings directly, attempting migration...");
        let mut settings = Settings::default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
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
                if let Ok(mut permission_settings) = serde_json::from_value::<PermissionSettings>(perms.clone()) {
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
            if let Some(len) = value.get("max_active_tool_output_length").and_then(|v| v.as_u64()) {
                settings.max_active_tool_output_length = len as usize;
            }
            if let Some(uid) = value.get("composio_user_id").and_then(|v| v.as_str()) {
                settings.composio_user_id = Some(uid.to_string());
            }
        }

        // After migrating, save the repaired settings file for the next run.
        if self.save(&settings).is_err() {
            tracing::error!("Failed to save migrated settings.");
        }

        settings
    }

    pub fn save(&self, settings: &Settings) -> Result<(), std::io::Error> {
        let content = serde_json::to_string_pretty(settings)?;
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.settings_path, content)
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
    About,
}

impl Default for SettingsTab {
    fn default() -> Self {
        Self::General
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UiState {
    pub settings_panel_width: f64,
    /// Whether to show tool call Arguments section by default
    #[serde(default = "default_true")]
    pub show_tool_arguments: bool,
    /// Whether to show tool call Response section by default
    #[serde(default)]
    pub show_tool_response: bool,
    /// Whether to show tool call Thinking Process section by default
    #[serde(default)]
    pub show_tool_thought: bool,
    /// MCP servers that are unloaded (tools hidden from AI)
    #[serde(default)]
    pub unloaded_mcp_servers: Vec<String>,
    /// Last active tab in Settings Panel
    #[serde(default)]
    pub active_settings_tab: SettingsTab,
    /// Whether LLM config section is collapsed
    #[serde(default)]
    pub llm_config_collapsed: bool,
    /// Whether MCP instructions are collapsed
    #[serde(default)]
    pub mcp_instructions_collapsed: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            settings_panel_width: 420.0,  // Comfortable width for 1440px window
            show_tool_arguments: true,
            show_tool_response: false,
            show_tool_thought: false,
            unloaded_mcp_servers: Vec::new(),
            active_settings_tab: SettingsTab::default(),
            llm_config_collapsed: false,
            mcp_instructions_collapsed: false,
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
}