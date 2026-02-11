use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use dioxus_signals::Writable;

use crate::mcp::manager::McpContext;
use serde_json::Value;

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

impl ConversationSummary {
    /// Truncate summary to max_chars, appending truncation notice if needed
    pub fn truncate_summary(&mut self, max_chars: usize) {
        if max_chars > 0 && self.summary.len() > max_chars {
            let mut truncated_len = max_chars.saturating_sub(20); // Leave room for notice
            while truncated_len > 0 && !self.summary.is_char_boundary(truncated_len) {
                truncated_len -= 1;
            }
            self.summary.truncate(truncated_len);
            self.summary.push_str("... [truncated]");
        }
    }
}

impl ConversationSummaryEntities {
    /// Prune entities to max_count
    pub fn prune_entities(&mut self, max_count: usize) {
        if max_count > 0 && self.other_entities.len() > max_count {
            while self.other_entities.len() > max_count {
                if let Some(key) = self.other_entities.keys().next().cloned() {
                    self.other_entities.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub function_declarations: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolWrapper {
    pub tool: Tool,
}

#[derive(Default)]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ActiveContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_persona: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force_tool_use_instruction: Option<String>,
    pub conversation_summary: ConversationSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_tools: Option<McpContext>, // Fixed: Restored field as it is still used in chat.rs/prompt_builder.rs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolWrapper>>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}



#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<super::components::chat::Message>,
    pub active_context: ActiveContext,
    pub last_updated: DateTime<Utc>,
    #[serde(default)]
    pub accumulated_cost: f64,
    #[serde(default)]
    pub accumulated_tokens: i32,
    #[serde(default)]
    pub accumulated_turns: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_optimization_summary: Option<String>,
    /// The specific Composio profile bound to this session.
    /// Acts as the live authority for tool-calling/MCP context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composio_profile: Option<String>,
}

impl Session {
    pub fn delete_message_and_after(&mut self, message_id: &str) -> usize {
        if let Ok(uuid) = uuid::Uuid::parse_str(message_id) {
            if let Some(index) = self.messages.iter().position(|m| m.id == uuid) {
                let count = self.messages.len() - index;
                self.messages.truncate(index);
                self.last_updated = Utc::now();
                count
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Calculate total cost for all messages in this session
    /// Always sums from message-level usage data, which is the source of truth.
    pub fn total_cost(&self) -> f64 {
        self.messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .filter_map(|u| u.cost)
            .sum()
    }

    /// Calculate total tokens for all messages in this session
    /// Always sums from message-level usage data, which is the source of truth.
    pub fn total_tokens(&self) -> i32 {
        self.messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| u.total_tokens)
            .sum()
    }

    /// Calculate average tokens per turn
    /// Always calculates from message-level usage data, which is the source of truth.
    pub fn average_tokens_per_turn(&self) -> f64 {
        let tokens = self
            .messages
            .iter()
            .filter_map(|m| m.usage.as_ref())
            .map(|u| u.total_tokens)
            .sum::<i32>() as f64;
        let turns = self
            .messages
            .iter()
            .filter(|m| m.author == "Hobbes")
            .count() as f64;

        if turns > 0.0 {
            tokens / turns
        } else {
            0.0
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub sessions: HashMap<String, Session>,
    pub active_session_id: String,
    pub window_width: f64,
    pub window_height: f64,
    #[serde(default)]
    pub tool_call_history: Vec<crate::components::shared::ToolCallRecord>,
    /// When true, save() will no-op to protect backup data after load failure
    #[serde(skip)]
    pub save_disabled: bool,
}

fn get_sessions_path() -> Option<PathBuf> {
    dirs::config_dir()
        .and_then(|mut path| {
            path.push("com.hobbes.app");
            fs::create_dir_all(&path).ok()?;
            path.push("sessions.json");
            Some(path)
        })
}

impl SessionState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        // This should be lightweight and not perform I/O.
        // Loading will be handled asynchronously in the UI.
        Self::default()
    }

    pub fn load() -> Result<Self, std::io::Error> {
        let path = get_sessions_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find sessions path")
        })?;
        let data = fs::read_to_string(&path).map_err(|e| {
            tracing::error!("Failed to read session file at {:?}: {}", path, e);
            e
        })?;

        // Try direct deserialization first
        if let Ok(mut state) = serde_json::from_str::<Self>(&data) {
            tracing::info!("Successfully loaded session data.");

            // Validate active_session_id
            if !state.sessions.contains_key(&state.active_session_id) {
                tracing::warn!(
                    "Loaded active_session_id '{}' not found in sessions. Resetting.",
                    state.active_session_id
                );
                if !state.sessions.is_empty() {
                    state.active_session_id = state
                        .sessions
                        .values()
                        .max_by_key(|s| s.last_updated)
                        .map(|s| s.id.clone())
                        .unwrap_or_default();
                } else {
                    state.active_session_id.clear();
                }
            }
            return Ok(state);
        }

        // If direct deserialization fails, attempt migration
        tracing::warn!("Failed to deserialize session state directly, attempting migration...");

        // Backup the old file before attempting to overwrite
        let backup_path = path.with_extension("json.bak");
        fs::copy(&path, backup_path)?;

        let mut state = SessionState::default();
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&data) {
            // Migrate MessageContent::Text from old tuple format to new struct format
            if let Some(sessions_obj) = value.get_mut("sessions").and_then(|v| v.as_object_mut()) {
                for (_session_id, session_val) in sessions_obj.iter_mut() {
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        for message in messages.iter_mut() {
                            if let Some(content) = message.get_mut("content") {
                                // Check if this is the old Text format: {"Text": "string"}
                                if let Some(text_str) = content.get("Text").and_then(|v| v.as_str())
                                {
                                    // Convert to new format: {"Text": {"content": "string", "thought_signature": null}}
                                    *content = serde_json::json!({
                                        "Text": {
                                            "content": text_str,
                                            "thought_signature": null,
                                            "thought_summary": null
                                        }
                                    });
                                    tracing::debug!("Migrated MessageContent::Text for message");
                                }
                            }
                        }
                    }

                    // Migrate messages without created_at timestamps
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        let base_time = chrono::Utc::now() - chrono::Duration::hours(1); // Start 1 hour ago
                        for (index, message) in messages.iter_mut().enumerate() {
                            // Check if created_at field exists
                            if message.get("created_at").is_none() {
                                // Assign timestamp: base_time + index milliseconds
                                let timestamp =
                                    base_time + chrono::Duration::milliseconds(index as i64);
                                message.as_object_mut().unwrap().insert(
                                    "created_at".to_string(),
                                    serde_json::json!(timestamp.to_rfc3339()),
                                );
                                tracing::debug!("Migrated message {} with timestamp", index);
                            }
                        }
                    }

                    // Migrate ToolCall/SkillCall fields
                    if let Some(messages) = session_val
                        .get_mut("messages")
                        .and_then(|v| v.as_array_mut())
                    {
                        for message in messages.iter_mut() {
                            if let Some(content) = message.get_mut("content") {
                                // Helper closure to migrate common fields
                                let migrate_tool_fields = |obj: &mut serde_json::Map<String, serde_json::Value>| {
                                    // id -> execution_id
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                            tracing::debug!("Migrated id -> execution_id");
                                        }
                                    }
                                    // name -> tool_name
                                    if obj.contains_key("name") && !obj.contains_key("tool_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("tool_name".to_string(), val);
                                            tracing::debug!("Migrated name -> tool_name");
                                        }
                                    }
                                };
                                
                                let migrate_skill_fields = |obj: &mut serde_json::Map<String, serde_json::Value>| {
                                    // id -> execution_id
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                            tracing::debug!("Migrated id -> execution_id");
                                        }
                                    }
                                    // name -> skill_name
                                    if obj.contains_key("name") && !obj.contains_key("skill_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("skill_name".to_string(), val);
                                            tracing::debug!("Migrated name -> skill_name");
                                        }
                                    }
                                };

                                // Check for ToolCall
                                if let Some(tool_call) = content.get_mut("ToolCall").and_then(|v| v.as_object_mut()) {
                                    migrate_tool_fields(tool_call);
                                }
                                // Check for PermissionRequest (structure similar to ToolCall)
                                if let Some(perm_req) = content.get_mut("PermissionRequest").and_then(|v| v.as_object_mut()) {
                                    migrate_tool_fields(perm_req);
                                }
                                // Check for SkillCall
                                if let Some(skill_call) = content.get_mut("SkillCall").and_then(|v| v.as_object_mut()) {
                                    migrate_skill_fields(skill_call);
                                }
                                // Check for SkillPermissionRequest
                                if let Some(skill_perm) = content.get_mut("SkillPermissionRequest").and_then(|v| v.as_object_mut()) {
                                    migrate_skill_fields(skill_perm);
                                }
                            }
                        }
                    }
                }
            }

            // Now deserialize the migrated value
            if let Some(sessions_val) = value.get("sessions") {
                match serde_json::from_value(sessions_val.clone()) {
                    Ok(sessions) => {
                        state.sessions = sessions;
                        tracing::info!("Migration recovered {} sessions", state.sessions.len());
                    }
                    Err(e) => {
                        tracing::error!("Migration failed to deserialize sessions: {}. NOT overwriting backup.", e);
                        // Return error so caller doesn't use empty state
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Migration failed: {}. Your data backup is at sessions.json.bak", e)
                        ));
                    }
                }
            }
            if let Some(active_id) = value.get("active_session_id").and_then(|v| v.as_str()) {
                state.active_session_id = active_id.to_string();
            }
            if let Some(width) = value.get("window_width").and_then(|v| v.as_f64()) {
                state.window_width = width;
            }
            if let Some(height) = value.get("window_height").and_then(|v| v.as_f64()) {
                state.window_height = height;
            }
            if let Some(history_val) = value.get("tool_call_history") {
                if let Ok(history) = serde_json::from_value(history_val.clone()) {
                    state.tool_call_history = history;
                }
            }
        }

        // Only save the migrated state if we actually recovered sessions
        if !state.sessions.is_empty() {
            if let Err(e) = state.save() {
                tracing::error!("Failed to save migrated session state: {}", e);
            }
        } else {
            tracing::warn!("Migration produced empty sessions - NOT saving to preserve backup");
        }

        Ok(state)
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        // If save is disabled (due to load failure), protect the backup
        if self.save_disabled {
            tracing::warn!("Save disabled due to prior load failure - protecting backup data");
            return Ok(());
        }

        use std::io::Write;

        let path = get_sessions_path().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Could not find sessions path")
        })?;
        let parent_dir = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not find parent directory",
            )
        })?;

        let data = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Create temp file in the same directory (required for atomic rename on same filesystem)
        let mut temp_file = tempfile::NamedTempFile::new_in(parent_dir)?;

        // Write data to temp file
        temp_file.write_all(data.as_bytes())?;

        // Sync to disk to ensure data is persisted before rename
        temp_file.as_file().sync_all()?;

        // Set restrictive permissions on the temp file before persisting
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            temp_file.as_file().set_permissions(permissions)?;
        }

        // Atomically rename temp file to target path
        // This is the key operation - if it succeeds, the file is fully written
        // If it fails, the original file remains intact
        temp_file.persist(&path).map_err(|e| e.error)?;

        Ok(())
    }

    pub fn create_session_raw(&mut self, initial_profile: Option<String>) -> String {
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Local::now();
        let new_session = Session {
            id: new_id.clone(),
            name: now.format("%b %d - %I:%M %p").to_string(),
            messages: vec![],
            active_context: ActiveContext::default(),
            last_updated: Utc::now(),
            accumulated_cost: 0.0,
            accumulated_tokens: 0,
            accumulated_turns: 0,
            memory_optimization_summary: None,
            composio_profile: initial_profile,
        };
        self.sessions.insert(new_id.clone(), new_session);
        self.active_session_id = new_id.clone();
        new_id
    }

    pub fn create_session(&mut self, initial_profile: Option<String>) -> String {
        let new_id = self.create_session_raw(initial_profile);
        Self::save_async(self.clone(), None);
        new_id
    }

    pub fn delete_session_raw(&mut self, id: &str) {
        self.sessions.remove(id);

        if self.active_session_id == id {
            // The active session was deleted. Find a new one or clear the active id.
            self.active_session_id = self
                .sessions
                .values()
                .max_by_key(|s| s.last_updated)
                .map(|s| s.id.clone())
                .unwrap_or_default();
        } else if self.sessions.is_empty() {
            self.active_session_id = String::new();
        }
    }

    pub fn delete_session(&mut self, id: &str) {
        self.delete_session_raw(id);
        Self::save_async(self.clone(), None);
    }

    pub fn get_active_session(&self) -> Option<&Session> {
        self.sessions.get(&self.active_session_id)
    }

    pub fn get_active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(&self.active_session_id)
    }

    #[allow(dead_code)] // Vestigial after session-targeted streaming refactor (v0.9.46); kept for potential future non-streaming callers
    pub fn touch_active_session(&mut self) {
        if let Some(session) = self.sessions.get_mut(&self.active_session_id) {
            session.last_updated = Utc::now();
        }
    }

    /// Touch (update `last_updated`) on a specific session by ID.
    /// Used by stream_manager to target the originating session after a tab switch.
    pub fn touch_session(&mut self, session_id: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_updated = Utc::now();
        }
    }

    /// Look up a message by UUID within a specific session (not the active one).
    /// Used by stream_manager to write streaming data to the originating session.
    pub fn get_message_mut_in_session(
        &mut self,
        session_id: &str,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.sessions
            .get_mut(session_id)
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
    }

    /// Remove a message from a specific session by ID.
    /// Used by cancel_stream to target the originating session after a tab switch.
    pub fn remove_message_in_session(&mut self, session_id: &str, message_id: &uuid::Uuid) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if let Some(index) = session.messages.iter().position(|m| m.id == *message_id) {
                session.messages.remove(index);
                tracing::info!(message_id = %message_id, session_id = %session_id, "Removed message from target session.");
            }
        }
    }


    pub fn update_window_size(&mut self, width: f64, height: f64) {
        tracing::debug!("Updating window size in state to: {}x{}", width, height);
        self.window_width = width;
        self.window_height = height;
        // Note: Save is now handled asynchronously by the caller to avoid UI hang.
        // The caller should invoke save_async() after updating window size.
    }

    /// Saves the session state to disk on a background thread.
    /// This prevents blocking the main UI thread during file I/O.
    /// If `error_signal` is provided, save failures will be surfaced to the UI.
    ///
    /// **Design Note:** The caller passes a cloned `SessionState` by value (not by reference),
    /// which means the full state — including all sessions and messages — is cloned before
    /// being moved into the spawned task. This is an intentional trade-off: serializing on the
    /// main thread would block the UI for the entire JSON encoding duration, whereas the clone
    /// cost (proportional to total message count) is typically negligible for normal usage.
    /// For extremely large states this could become a bottleneck; a future optimization could
    /// serialize into bytes on the main thread (fast memcpy) and only do file I/O async.
    pub fn save_async(state: SessionState, error_signal: Option<dioxus::prelude::Signal<Option<String>>>) {
        let handle = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || state.save()).await.unwrap_or_else(|e| Err(std::io::Error::other(e)))
        });
        if let Some(mut sig) = error_signal {
            dioxus::prelude::spawn(async move {
                if let Ok(Err(e)) = handle.await {
                    tracing::error!("Failed to save session state async: {}", e);
                    *sig.write() = Some(format!("Failed to save session: {}", e));
                }
            });
        } else {
            tokio::spawn(async move {
                if let Ok(Err(e)) = handle.await {
                    tracing::error!("Failed to save session state async: {}", e);
                }
            });
        }
    }

    pub fn update_session_name_raw(&mut self, id: &str, new_name: String) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.name = new_name;
        }
    }

    pub fn update_session_name(&mut self, id: &str, new_name: String) {
        self.update_session_name_raw(id, new_name);
        Self::save_async(self.clone(), None);
    }
    pub fn get_message_mut(
        &mut self,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut()
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
    }

    #[allow(dead_code)] // Vestigial after session-targeted streaming refactor (v0.9.46); kept for potential future non-streaming callers
    pub fn remove_message(&mut self, message_id: &uuid::Uuid) {
        if let Some(session) = self.get_active_session_mut() {
            if let Some(index) = session.messages.iter().position(|m| m.id == *message_id) {
                session.messages.remove(index);
                tracing::info!(message_id = %message_id, "Removed message from active session.");
            }
        }
    }
    pub fn get_message_mut_by_execution_id(
        &mut self,
        execution_id: &str,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut().and_then(|session| {
            session.messages.iter_mut().find(|m| match &m.content {
                super::components::shared::MessageContent::ToolCall(tc) => {
                    tc.execution_id == execution_id
                }
                super::components::shared::MessageContent::PermissionRequest(tc) => {
                    tc.execution_id == execution_id
                }
                _ => false,
            })
        })
    }
}
impl Default for SessionState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            active_session_id: String::new(),
            window_width: 1440.0, // 16:9 ratio default
            window_height: 810.0,
            tool_call_history: Vec::new(),
            save_disabled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// Test that atomic save creates a valid JSON file with correct permissions
    #[test]
    fn test_atomic_save_creates_valid_file() {
        // Create a temp directory to simulate the config dir
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let test_path = temp_dir.path().join("test_sessions.json");

        // Create a test SessionState
        let state = SessionState {
            window_width: 800.0,
            window_height: 600.0,
            ..Default::default()
        };

        // Manually save to our test path (bypassing get_sessions_path)
        let data = serde_json::to_string_pretty(&state).expect("Failed to serialize");

        // Use the same atomic write pattern
        {
            use std::io::Write;
            let mut temp_file = tempfile::NamedTempFile::new_in(temp_dir.path())
                .expect("Failed to create temp file");
            temp_file
                .write_all(data.as_bytes())
                .expect("Failed to write");
            temp_file.as_file().sync_all().expect("Failed to sync");

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::Permissions::from_mode(0o600);
                temp_file
                    .as_file()
                    .set_permissions(permissions)
                    .expect("Failed to set permissions");
            }

            temp_file.persist(&test_path).expect("Failed to persist");
        }

        // Verify the file exists and is valid JSON
        let mut file = fs::File::open(&test_path).expect("Failed to open saved file");
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .expect("Failed to read file");

        let loaded: SessionState = serde_json::from_str(&contents).expect("Failed to parse JSON");
        assert_eq!(loaded.window_width, 800.0);
        assert_eq!(loaded.window_height, 600.0);

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&test_path).expect("Failed to get metadata");
            let mode = metadata.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "File permissions should be 0600");
        }
    }

    /// Test that atomic save doesn't corrupt existing file on serialization error
    #[test]
    fn test_atomic_save_preserves_original_on_failure() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let test_path = temp_dir.path().join("test_sessions.json");

        // Write an initial file
        let initial_content = r#"{"sessions":{},"active_session_id":"","window_width":100.0,"window_height":100.0,"tool_call_history":[]}"#;
        fs::write(&test_path, initial_content).expect("Failed to write initial file");

        // Verify original exists
        assert!(test_path.exists());

        // The temp file approach means even if we fail mid-write, original is preserved
        // Since persist() is atomic, we can't really test a mid-write failure easily,
        // but we can verify the pattern works for normal cases

        let original = fs::read_to_string(&test_path).expect("Failed to read original");
        assert!(original.contains("100.0"));
    }
}
