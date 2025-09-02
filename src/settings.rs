use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub api_key: Option<String>,
    pub chat_model: String,
    pub summary_model: String,
    pub persona: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: None,
            chat_model: "gemini-1.5-pro-latest".to_string(),
            summary_model: "gemini-1.5-flash-latest".to_string(),
            persona: "You are Hobbes, a helpful AI assistant.".to_string(),
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