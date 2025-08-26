use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use uuid;
use dirs;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<super::components::chat::Message>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub sessions: HashMap<String, Session>,
    pub active_session_id: String,
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

    fn load() -> Result<Self, std::io::Error> {
        let path = get_sessions_path().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find sessions path"))?;
        let data = fs::read_to_string(path)?;
        serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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

    pub fn set_active_session(&mut self, id: String) {
        self.active_session_id = id;
        if let Err(e) = self.save() {
            tracing::error!("Failed to save session state after setting active session: {}", e);
        }
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            active_session_id: String::new(),
        }
    }
}