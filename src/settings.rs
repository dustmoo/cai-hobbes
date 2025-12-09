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
pub struct Settings {
    pub active_llm: LlmProvider,
    pub gemini_config: GeminiConfig,
    pub qdrant_url: Option<String>,
    pub persona: String,
    pub user_name: Option<String>,
    pub force_tool_use_instruction: Option<String>,
    pub project_folder: Option<String>,
    pub chat_history_length: usize,
    pub show_tray_icon: bool,
    pub global_hotkey: String,
    pub permission_settings: PermissionSettings,
    #[serde(default = "default_true")]
    pub confirm_on_delete: bool,
    #[serde(default = "default_true")]
    pub confirm_on_save: bool,
    #[serde(default = "default_true")]
    pub confirm_on_message_delete: bool,
    #[serde(skip)]
    pub smithery_api_key: Option<String>,
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
            qdrant_url: None,
            persona: "You are Hobbes. Be a direct, clear, and radically candid partner. Function as an exocortex, matching the user's communication style. Your default tone is that of a professional friend.".to_string(),
            user_name: None,
            force_tool_use_instruction: Some("You must always use the provided tools to answer the user's request, even if you think you know the answer. Do not answer from your own knowledge base when tools are available. When using the fetch tool, you MUST provide markdown links as sources.".to_string()),
            project_folder: None,
            chat_history_length: 8,
            show_tray_icon: true,
            global_hotkey: "CmdOrCtrl+Shift+H".to_string(),
            permission_settings: PermissionSettings {
                auto_approval_enabled: true,
                granular_permissions,
                max_ai_turns: 25,
            },
            confirm_on_delete: true,
            confirm_on_save: true,
            confirm_on_message_delete: true,
            smithery_api_key: None,
        }
    }
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
        if let Ok(settings) = serde_json::from_str(&content) {
            return settings;
        }

        // If direct deserialization fails, try a field-by-field migration.
        tracing::warn!("Failed to deserialize settings directly, attempting migration...");
        let mut settings = Settings::default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(qdrant_url) = value.get("qdrant_url").and_then(|v| v.as_str()) {
                settings.qdrant_url = Some(qdrant_url.to_string());
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
                settings.global_hotkey = hotkey.to_string();
            }
            // Note: Complex nested structs like permission_settings are harder to migrate
            // field-by-field and will fall back to default if they fail to parse.
            if let Some(perms) = value.get("permission_settings") {
                if let Ok(permission_settings) = serde_json::from_value(perms.clone()) {
                    settings.permission_settings = permission_settings;
                }
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UiState {
    pub settings_panel_width: f64,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            settings_panel_width: 256.0,
        }
    }
}

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