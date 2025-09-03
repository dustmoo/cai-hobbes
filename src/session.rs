use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use uuid;
use dirs;

use serde_json::Value;
use crate::mcp::manager::McpContext;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummaryEntities {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_name: String,
    #[serde(flatten)]
    pub other_entities: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummary {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sentiment: String,
    #[serde(default)]
    pub entities: ConversationSummaryEntities,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ActiveContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_persona: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_instruction: Option<String>,
    pub conversation_summary: ConversationSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tools: Option<McpContext>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl Default for ActiveContext {
    fn default() -> Self {
        Self {
            system_persona: None,
            user_instruction: None,
            conversation_summary: ConversationSummary::default(),
            mcp_tools: None,
            extra: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<super::components::chat::Message>,
    pub active_context: ActiveContext,
    pub last_updated: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub sessions: HashMap<String, Session>,
    pub active_session_id: String,
    pub window_width: f64,
    pub window_height: f64,
}

fn get_sessions_path() -> Option<PathBuf> {
    dirs::config_dir().map(|mut path| {
        path.push("cai-hobbes");
        fs::create_dir_all(&path).ok()?;
        path.push("sessions.json");
        Some(path)
    }).flatten()
}

impl SessionState {
    pub fn new() -> Self {
        Self::load().unwrap_or_else(|_| {
            let new_state = Self::default();
            if let Err(e) = new_state.save() {
                tracing::error!("Failed to save initial session state: {}", e);
            }
            new_state
        })
    }

    pub fn load() -> Result<Self, std::io::Error> {
        let path = get_sessions_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find sessions path"))?;
        let data = fs::read_to_string(path)?;
        let state: Self = serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tracing::info!("Loaded window size: {}x{}", state.window_width, state.window_height);
        Ok(state)
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = get_sessions_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find sessions path"))?;
        let data = serde_json::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, data)
    }

    pub fn create_session(&mut self) {
        let new_id = uuid::Uuid::new_v4().to_string();
        let new_session = Session {
            id: new_id.clone(),
            name: format!("Chat {}", self.sessions.len() + 1),
            messages: vec![],
            active_context: ActiveContext::default(),
            last_updated: Utc::now(),
        };
        self.sessions.insert(new_id.clone(), new_session);
        self.active_session_id = new_id;
        if let Err(e) = self.save() {
            tracing::error!("Failed to save session state after creating session: {}", e);
        }
    }

    pub fn delete_session(&mut self, id: &str) {
        self.sessions.remove(id);

        if self.active_session_id == id {
            // The active session was deleted. Find a new one or clear the active id.
            self.active_session_id = self.sessions.keys().next().cloned().unwrap_or_default();
        } else if self.sessions.is_empty() {
            self.active_session_id = String::new();
        }

        if let Err(e) = self.save() {
            tracing::error!("Failed to save session state after deleting session: {}", e);
        }
    }

    pub fn get_active_session(&self) -> Option<&Session> {
        self.sessions.get(&self.active_session_id)
    }

    pub fn get_active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(&self.active_session_id)
    }

    pub fn touch_active_session(&mut self) {
        if let Some(session) = self.sessions.get_mut(&self.active_session_id) {
            session.last_updated = Utc::now();
        }
    }
    pub fn set_active_session(&mut self, id: String) {
        self.active_session_id = id;
        if let Err(e) = self.save() {
            tracing::error!("Failed to save session state after setting active session: {}", e);
        }
    }

    pub fn update_window_size(&mut self, width: f64, height: f64) {
        tracing::info!("Updating window size in state to: {}x{}", width, height);
        self.window_width = width;
        self.window_height = height;
        if let Err(e) = self.save() {
            tracing::error!("Failed to save session state after updating window size: {}", e);
        }
    }

    pub fn update_session_name(&mut self, id: &str, new_name: String) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.name = new_name;
            if let Err(e) = self.save() {
                tracing::error!("Failed to save session state after updating session name: {}", e);
            }
        }
    }
}
impl Default for SessionState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            active_session_id: String::new(),
            window_width: 675.0,
            window_height: 750.0,
        }
    }
}