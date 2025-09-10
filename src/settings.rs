use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub api_key: Option<String>,
    pub chat_model: String,
    pub summary_model: String,
    pub persona: String,
    pub force_tool_use_instruction: Option<String>,
    pub project_folder: Option<String>,
    pub settings_panel_width: Option<f64>,
    pub chat_history_length: usize,
    pub show_tray_icon: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: None,
            chat_model: "gemini-2.5-pro".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            persona: "You are Hobbes, a helpful AI assistant.".to_string(),
            force_tool_use_instruction: Some("You must always use the provided tools to answer the user's request, even if you think you know the answer. Do not answer from your own knowledge base when tools are available. When using the fetch tool, you MUST provide markdown links as sources.".to_string()),
            project_folder: None,
            settings_panel_width: Some(256.0),
            chat_history_length: 4,
            show_tray_icon: true,
        }
    }
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
            return Settings::default();
        }

        fs::read_to_string(&self.settings_path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, settings: &Settings) -> Result<(), std::io::Error> {
        let content = serde_json::to_string_pretty(settings)?;
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.settings_path, content)
    }
}