use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use serde::{Deserialize, Serialize};

/// Path to the local context store file
const CONTEXT_STORE_FILENAME: &str = "composio_context_store.json";

/// Nested storage structure: Toolkit Slug -> User ID -> Key -> Value
/// Example: "clickup" -> "user_123" -> "team_id" -> "90174"
type ContextMap = HashMap<String, HashMap<String, HashMap<String, String>>>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ContextStoreData {
    contexts: ContextMap,
}

#[derive(Debug, Clone)]
pub struct ContextStore {
    data: Arc<RwLock<ContextStoreData>>,
    file_path: PathBuf,
}

impl ContextStore {
    /// Initialize the context store, loading from disk if available
    pub fn new() -> Self {
        let mut path = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
        path.push(".gemini");
        path.push("antigravity");
        
        // Ensure directory exists
        if let Err(e) = fs::create_dir_all(&path) {
            tracing::error!("Failed to create context store directory: {}", e);
        }
        
        path.push(CONTEXT_STORE_FILENAME);
        
        let store = Self {
            data: Arc::new(RwLock::new(ContextStoreData::default())),
            file_path: path.clone(),
        };
        
        if path.exists() {
            if let Err(e) = store.load() {
                tracing::warn!("Failed to load context store from {:?}: {}", path, e);
            }
        }
        
        store
    }
    
    /// Load data from disk
    fn load(&self) -> std::io::Result<()> {
        let content = fs::read_to_string(&self.file_path)?;
        let data: ContextStoreData = serde_json::from_str(&content)?;
        
        let mut lock = self.data.write().unwrap();
        *lock = data;
        Ok(())
    }
    
    /// Save data to disk
    fn save(&self) -> std::io::Result<()> {
        let lock = self.data.read().unwrap();
        let content = serde_json::to_string_pretty(&*lock)?;
        fs::write(&self.file_path, content)
    }
    
    /// Save a specific context parameter for a toolkit and user
    pub fn save_param(&self, toolkit: &str, user_id: &str, key: &str, value: &str) {
        // SECURITY WARNING: Storing context keys locally
        tracing::warn!("[SECURITY NOTICE] Storing context parameter '{}' for toolkit '{}' in local context store.", key, toolkit);
        
        let mut lock = self.data.write().unwrap();
        
        lock.contexts
            .entry(toolkit.to_lowercase())
            .or_default()
            .entry(user_id.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
            
        drop(lock); // Release lock before I/O
        
        if let Err(e) = self.save() {
            tracing::error!("Failed to persist context store: {}", e);
        }
    }
    
    /// Get all stored context parameters for a toolkit and user
    pub fn get_context(&self, toolkit: &str, user_id: &str) -> Option<HashMap<String, String>> {
        let lock = self.data.read().unwrap();
        
        lock.contexts
            .get(&toolkit.to_lowercase())
            .and_then(|user_map| user_map.get(user_id))
            .cloned()
    }
    
    /// Get the path to the store file (for debugging/info)
    #[allow(dead_code)]
    pub fn get_file_path(&self) -> String {
        self.file_path.to_string_lossy().to_string()
    }
}
