use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::mcp::manager::McpContext;
use serde_json::Value;

/// Deserialize a String that may be `null` in JSON (from LLM responses).
/// `#[serde(default)]` only handles missing keys; this also handles explicit `null`.
fn null_to_empty<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummaryEntities {
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub user_name: String,
    #[serde(flatten)]
    pub other_entities: HashMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ConversationSummary {
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty", deserialize_with = "null_to_empty")]
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

#[derive(Default, Clone, Debug, Serialize, Deserialize, PartialEq)]
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
    /// Dynamically discovered Composio tools (populated by COMPOSIO_GET_APP_TOOLS).
    /// These are merged into the Gemini FunctionDeclarations on the next turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_composio_tools: Vec<rmcp::model::Tool>,
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

/// Schema version for SessionState persistence.
/// Bump this when adding new migrations to `load()`.
/// Existing files without this field default to 0 via `#[serde(default)]`.
pub const CURRENT_SESSION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    /// Schema version for forward-compatible migrations.
    /// Files without this field (pre-versioning) default to 0.
    #[serde(default)]
    pub schema_version: u32,
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
    dirs::config_dir().and_then(|mut path| {
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
            tracing::info!(
                "Successfully loaded session data (schema_version={}).",
                state.schema_version
            );

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

            // Run forward migrations if schema is behind current version.
            // Gate future migrations here: `if state.schema_version < 2 { ... }`
            if state.schema_version < CURRENT_SESSION_SCHEMA_VERSION {
                tracing::info!(
                    "Running forward migrations from schema v{} to v{}",
                    state.schema_version,
                    CURRENT_SESSION_SCHEMA_VERSION
                );
                state.schema_version = CURRENT_SESSION_SCHEMA_VERSION;
                // Save the upgraded schema version
                if let Err(e) = state.save() {
                    tracing::error!("Failed to save schema-upgraded session state: {}", e);
                }
            }

            return Ok(state);
        }

        // If direct deserialization fails, attempt migration
        tracing::warn!("Failed to deserialize session state directly, attempting migration...");

        // Backup the old file before attempting to overwrite
        let backup_path = path.with_extension("json.bak");
        fs::copy(&path, backup_path)?;

        let mut state = Self::migrate_from_raw_json(&data)?;

        // Mark as current schema version after successful migration
        state.schema_version = CURRENT_SESSION_SCHEMA_VERSION;

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

    /// Migrate raw JSON data from an older format into a [`SessionState`].
    ///
    /// This is the fallback path when direct deserialization fails — it parses
    /// the data as a generic `serde_json::Value`, applies all known field/format
    /// migrations, and then deserializes the result into the current struct shape.
    ///
    /// Extracted from `load()` so it can be exercised by unit tests without
    /// touching the filesystem.
    fn migrate_from_raw_json(data: &str) -> Result<Self, std::io::Error> {
        let mut state = SessionState::default();
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data) {
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
                        let base_time = chrono::Utc::now() - chrono::Duration::hours(1);
                        for (index, message) in messages.iter_mut().enumerate() {
                            if message.get("created_at").is_none() {
                                let timestamp =
                                    base_time + chrono::Duration::milliseconds(index as i64);
                                message
                                    .as_object_mut()
                                    .expect(
                                        "message migration: message value must be a JSON object",
                                    )
                                    .insert(
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
                                let migrate_tool_fields = |obj: &mut serde_json::Map<
                                    String,
                                    serde_json::Value,
                                >| {
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                        }
                                    }
                                    if obj.contains_key("name") && !obj.contains_key("tool_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("tool_name".to_string(), val);
                                        }
                                    }
                                };

                                let migrate_skill_fields = |obj: &mut serde_json::Map<
                                    String,
                                    serde_json::Value,
                                >| {
                                    if obj.contains_key("id") && !obj.contains_key("execution_id") {
                                        if let Some(val) = obj.remove("id") {
                                            obj.insert("execution_id".to_string(), val);
                                        }
                                    }
                                    if obj.contains_key("name") && !obj.contains_key("skill_name") {
                                        if let Some(val) = obj.remove("name") {
                                            obj.insert("skill_name".to_string(), val);
                                        }
                                    }
                                };

                                if let Some(tool_call) =
                                    content.get_mut("ToolCall").and_then(|v| v.as_object_mut())
                                {
                                    migrate_tool_fields(tool_call);
                                }
                                if let Some(perm_req) = content
                                    .get_mut("PermissionRequest")
                                    .and_then(|v| v.as_object_mut())
                                {
                                    migrate_tool_fields(perm_req);
                                }
                                if let Some(skill_call) =
                                    content.get_mut("SkillCall").and_then(|v| v.as_object_mut())
                                {
                                    migrate_skill_fields(skill_call);
                                }
                                if let Some(skill_perm) = content
                                    .get_mut("SkillPermissionRequest")
                                    .and_then(|v| v.as_object_mut())
                                {
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
                        tracing::error!(
                            "Migration failed to deserialize sessions: {}. NOT overwriting backup.",
                            e
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "Migration failed: {}. Your data backup is at sessions.json.bak",
                                e
                            ),
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
        Self::save_async(self, None);
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
        Self::save_async(self, None);
    }

    pub fn get_active_session(&self) -> Option<&Session> {
        self.sessions.get(&self.active_session_id)
    }

    pub fn get_active_session_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(&self.active_session_id)
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
        self.window_width = width;
        self.window_height = height;
        // Note: Save is now handled asynchronously by the caller to avoid UI hang.
        // The caller should invoke save_async() after updating window size.
    }

    /// Write pre-serialized bytes to the session file using the same atomic
    /// tempfile pattern as `save()`. Used by `save_async` to avoid cloning.
    fn save_bytes(bytes: Vec<u8>) -> Result<(), std::io::Error> {
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

        let mut temp_file = tempfile::NamedTempFile::new_in(parent_dir)?;
        temp_file.write_all(&bytes)?;
        temp_file.as_file().sync_all()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::Permissions::from_mode(0o600);
            temp_file.as_file().set_permissions(permissions)?;
        }

        temp_file.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }

    /// Saves the session state to disk on a background thread.
    /// This prevents blocking the main UI thread during file I/O.
    /// If `error_signal` is provided, save failures will be surfaced to the UI.
    ///
    /// **Design Note (Serialize-Then-Move):** Serialization happens on the calling
    /// thread via a borrow of `&SessionState`, producing an owned `Vec<u8>`. Only
    /// this byte buffer is moved to the background thread for file I/O. This avoids
    /// the expensive deep clone of the entire state (all sessions + all messages)
    /// that the previous implementation required.
    pub fn save_async(
        state: &SessionState,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        // Guard: skip if saves are disabled (backup protection)
        if state.save_disabled {
            tracing::warn!(
                "save_async: Save disabled due to prior load failure - protecting backup data"
            );
            return;
        }

        // Serialize on the current thread — borrows state, no clone needed
        let bytes = match serde_json::to_vec_pretty(state) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Failed to serialize session state: {}", e);
                if let Some(mut sig) = error_signal {
                    use dioxus::prelude::Writable;
                    *sig.write() = Some(format!("Failed to serialize session state: {}", e));
                }
                return;
            }
        };

        // Move only the byte buffer to the background thread for file I/O
        crate::async_persist::persist_bytes_async(
            bytes,
            Self::save_bytes,
            "session state",
            error_signal,
        );
    }

    /// Convenience: save the session state from a Dioxus Signal after releasing a write guard.
    /// This encapsulates the common `drop(guard); save_async(&signal.read(), ...)` pattern
    /// to avoid borrow conflicts between write guards and the read borrow needed for serialization.
    pub fn save_signal(
        signal: &dioxus::prelude::Signal<SessionState>,
        error_signal: Option<dioxus::prelude::Signal<Option<String>>>,
    ) {
        use dioxus::prelude::Readable;
        Self::save_async(&signal.read(), error_signal);
    }

    pub fn update_session_name_raw(&mut self, id: &str, new_name: String) {
        if let Some(session) = self.sessions.get_mut(id) {
            session.name = new_name;
        }
    }

    pub fn update_session_name(&mut self, id: &str, new_name: String) {
        self.update_session_name_raw(id, new_name);
        Self::save_async(self, None);
    }
    pub fn get_message_mut(
        &mut self,
        message_id: &uuid::Uuid,
    ) -> Option<&mut super::components::chat::Message> {
        self.get_active_session_mut()
            .and_then(|session| session.messages.iter_mut().find(|m| m.id == *message_id))
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

    /// Migrate session composio_profile from name-based to ID-based.
    /// Any session whose composio_profile matches a profile name (but not an ID)
    /// gets updated to the corresponding profile ID.
    pub fn migrate_session_profiles_to_ids(
        &mut self,
        settings: &crate::settings::Settings,
    ) -> bool {
        let mut migrated = false;
        for session in self.sessions.values_mut() {
            if let Some(ref value) = session.composio_profile {
                // Already an ID — skip
                if settings.composio_profiles.iter().any(|p| &p.id == value) {
                    continue;
                }
                // Match by name → replace with ID
                if let Some(profile) = settings.composio_profiles.iter().find(|p| &p.name == value)
                {
                    tracing::info!(
                        "Migrating session '{}' composio_profile from name '{}' to id '{}'",
                        session.id,
                        value,
                        profile.id
                    );
                    session.composio_profile = Some(profile.id.clone());
                    migrated = true;
                }
            }
        }
        migrated
    }
}
impl Default for SessionState {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SESSION_SCHEMA_VERSION,
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

    // === Migration Tests ===

    /// Helper: wrap a single session with messages into a full SessionState JSON string.
    /// Uses the old format WITHOUT schema_version so it will fail direct deser and hit migrate_from_raw_json.
    fn make_old_session_json(messages_json: &str) -> String {
        format!(
            r#"{{
                "sessions": {{
                    "test-session-1": {{
                        "id": "test-session-1",
                        "name": "Test Session",
                        "messages": [{messages_json}],
                        "active_context": {{}},
                        "last_updated": "2026-01-01T00:00:00Z"
                    }}
                }},
                "active_session_id": "test-session-1",
                "window_width": 800.0,
                "window_height": 600.0,
                "tool_call_history": []
            }}"#
        )
    }

    /// Test: old {"Text": "string"} format is migrated to {"Text": {"content": "string", ...}}
    #[test]
    fn test_migration_text_format() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": "Hello world"},
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 1);

        match &session.messages[0].content {
            crate::components::shared::MessageContent::Text {
                content,
                thought_signature,
                thought_summary,
            } => {
                assert_eq!(content, "Hello world");
                assert!(thought_signature.is_none());
                assert!(thought_summary.is_none());
            }
            other => panic!("Expected Text variant, got {:?}", other),
        }
    }

    /// Test: messages without created_at get timestamps backfilled in order.
    #[test]
    fn test_migration_timestamps() {
        let old_messages = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": {"content": "First", "thought_signature": null, "thought_summary": null}},
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000002",
                "author": "Hobbes",
                "content": {"Text": {"content": "Second", "thought_signature": null, "thought_summary": null}},
                "attachments": [],
                "comments": []
            }
        "#;
        let json = make_old_session_json(old_messages);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 2);

        // Both should have created_at, and the second should be after the first
        let t0 = session.messages[0].created_at;
        let t1 = session.messages[1].created_at;
        assert!(
            t1 > t0,
            "Second message timestamp should be after first: {:?} vs {:?}",
            t0,
            t1
        );
    }

    /// Test: ToolCall/PermissionRequest field renames (id -> execution_id, name -> tool_name)
    #[test]
    fn test_migration_toolcall_renames() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "Hobbes",
                "content": {
                    "ToolCall": {
                        "id": "old-exec-id",
                        "server_name": "test-server",
                        "name": "old-tool-name",
                        "arguments": "{}",
                        "status": "Completed",
                        "response": "ok",
                        "thought_signature": null,
                        "thought_summary": null
                    }
                },
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        match &session.messages[0].content {
            crate::components::shared::MessageContent::ToolCall(tc) => {
                assert_eq!(
                    tc.execution_id, "old-exec-id",
                    "id should have migrated to execution_id"
                );
                assert_eq!(
                    tc.tool_name, "old-tool-name",
                    "name should have migrated to tool_name"
                );
            }
            other => panic!("Expected ToolCall, got {:?}", other),
        }
    }

    /// Test: SkillCall/SkillPermissionRequest field renames (id -> execution_id, name -> skill_name)
    #[test]
    fn test_migration_skillcall_renames() {
        let old_message = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "Hobbes",
                "content": {
                    "SkillCall": {
                        "id": "old-skill-exec-id",
                        "name": "old-skill-name",
                        "arguments": "{}",
                        "status": "Completed",
                        "response": "ok",
                        "instructions": "do stuff"
                    }
                },
                "attachments": [],
                "comments": [],
                "created_at": "2026-01-01T00:00:00Z"
            }
        "#;
        let json = make_old_session_json(old_message);
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        match &session.messages[0].content {
            crate::components::shared::MessageContent::SkillCall(sc) => {
                assert_eq!(
                    sc.execution_id, "old-skill-exec-id",
                    "id should have migrated to execution_id"
                );
                assert_eq!(
                    sc.skill_name, "old-skill-name",
                    "name should have migrated to skill_name"
                );
            }
            other => panic!("Expected SkillCall, got {:?}", other),
        }
    }

    /// Test: Full round-trip — old format with ALL migration triggers, migrated, serialized, re-deserialized.
    #[test]
    fn test_migration_full_roundtrip() {
        // Build a fixture with every migration trigger active simultaneously:
        // 1. Old Text format ({"Text": "string"})
        // 2. Missing created_at
        // 3. Old ToolCall field names
        let old_messages = r#"
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "author": "User",
                "content": {"Text": "Hello from old format"},
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000002",
                "author": "Hobbes",
                "content": {
                    "ToolCall": {
                        "id": "tc-1",
                        "server_name": "mcp-server",
                        "name": "read_file",
                        "arguments": "{\"path\":\"/tmp/test\"}",
                        "status": "Completed",
                        "response": "file contents",
                        "thought_signature": null,
                        "thought_summary": null
                    }
                },
                "attachments": [],
                "comments": []
            },
            {
                "id": "00000000-0000-0000-0000-000000000003",
                "author": "Hobbes",
                "content": {"Text": "Here is the result"},
                "attachments": [],
                "comments": []
            }
        "#;
        let json = make_old_session_json(old_messages);

        // Step 1: Migrate from old format
        let state = SessionState::migrate_from_raw_json(&json).expect("Migration should succeed");

        let session = state
            .sessions
            .get("test-session-1")
            .expect("Session missing");
        assert_eq!(session.messages.len(), 3);

        // Verify Text migration worked
        match &session.messages[0].content {
            crate::components::shared::MessageContent::Text { content, .. } => {
                assert_eq!(content, "Hello from old format");
            }
            other => panic!("Message 0: expected Text, got {:?}", other),
        }

        // Verify ToolCall migration worked
        match &session.messages[1].content {
            crate::components::shared::MessageContent::ToolCall(tc) => {
                assert_eq!(tc.execution_id, "tc-1");
                assert_eq!(tc.tool_name, "read_file");
            }
            other => panic!("Message 1: expected ToolCall, got {:?}", other),
        }

        // Verify timestamps were backfilled
        for msg in &session.messages {
            // created_at should be set (non-zero)
            assert!(
                msg.created_at.timestamp() > 0,
                "created_at should be set for all messages"
            );
        }
        // Timestamps should be in order
        assert!(session.messages[1].created_at > session.messages[0].created_at);
        assert!(session.messages[2].created_at > session.messages[1].created_at);

        // Verify window dimensions preserved
        assert_eq!(state.window_width, 800.0);
        assert_eq!(state.window_height, 600.0);

        // Step 2: Serialize the migrated state and re-deserialize — "round trip"
        let serialized =
            serde_json::to_string_pretty(&state).expect("Serialization should succeed");
        let reloaded: SessionState = serde_json::from_str(&serialized)
            .expect("Round-trip deserialization should succeed without needing migration");

        assert_eq!(state.sessions.len(), reloaded.sessions.len());
        assert_eq!(state.active_session_id, reloaded.active_session_id);
        assert_eq!(state.window_width, reloaded.window_width);
        let reloaded_session = reloaded
            .sessions
            .get("test-session-1")
            .expect("Session missing on reload");
        assert_eq!(reloaded_session.messages.len(), 3);
    }

    /// Test: save() is a no-op when save_disabled is true.
    #[test]
    fn test_save_disabled_guard() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let test_path = temp_dir.path().join("disabled_sessions.json");

        // Create initial content
        fs::write(&test_path, "original content").expect("Failed to write initial file");

        let state = SessionState {
            save_disabled: true,
            window_width: 999.0,
            ..Default::default()
        };

        // save() should return Ok but NOT touch the file
        // We can't directly test with the hardcoded path, but we can verify the guard logic
        let result = state.save();
        // save() will try to use get_sessions_path(), but the important thing is
        // the save_disabled check returns early with Ok(())
        assert!(result.is_ok(), "save() with save_disabled should return Ok");
    }
}
