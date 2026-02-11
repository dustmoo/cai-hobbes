use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// A simple sidecar store for sustaining tool context (e.g. team_id, workspace_url)
/// that cannot be derived from the auth token alone.
///
/// This is used to inject parameters into tool execution arguments for routed toolkits.
#[derive(Debug, Clone)]
pub struct ContextStore {
    #[allow(dead_code)]
    profile_id: String,
    file_path: PathBuf,
    // Map<ToolkitSlug, Map<UserId, Map<Key, Value>>>
    // Deliberate triple-nested map: ToolkitSlug → UserId → Key → Value.
    #[allow(clippy::type_complexity)]
    cache: Arc<RwLock<HashMap<String, HashMap<String, HashMap<String, String>>>>>,
}

#[derive(Serialize, Deserialize)]
struct StoredContext {
    // Map<ToolkitSlug, Map<UserId, Map<Key, Value>>>
    data: HashMap<String, HashMap<String, HashMap<String, String>>>,
}

impl ContextStore {
    pub fn new(profile_id: &str) -> Self {
        let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        path.push("context_store.json");

        let store = Self {
            profile_id: profile_id.to_string(),
            file_path: path,
            cache: Arc::new(RwLock::new(HashMap::new())),
        };

        store.load();
        store
    }

    fn load(&self) {
        if let Ok(content) = fs::read_to_string(&self.file_path) {
            if let Ok(stored) = serde_json::from_str::<StoredContext>(&content) {
                if let Ok(mut cache) = self.cache.write() {
                    *cache = stored.data;
                }
            }
        }
    }

    fn save(&self) {
        if let Ok(cache) = self.cache.read() {
            let stored = StoredContext {
                data: cache.clone(),
            };
            if let Ok(content) = serde_json::to_string_pretty(&stored) {
                let _ = fs::write(&self.file_path, content);
            }
        }
    }

    pub fn save_param(&self, toolkit_slug: &str, user_id: &str, key: &str, value: &str) {
        let mut cache = self.cache.write().unwrap();

        let toolkit_entry = cache.entry(toolkit_slug.to_string()).or_default();
        let user_entry = toolkit_entry.entry(user_id.to_string()).or_default();

        user_entry.insert(key.to_string(), value.to_string());

        // Release lock before saving to avoid deadlocks (though save() takes a read lock)
        drop(cache);
        self.save();
    }

    pub fn get_context(
        &self,
        toolkit_slug: &str,
        user_id: &str,
    ) -> Option<HashMap<String, String>> {
        let cache = self.cache.read().unwrap();

        if let Some(toolkit_entry) = cache.get(toolkit_slug) {
            if let Some(user_entry) = toolkit_entry.get(user_id) {
                return Some(user_entry.clone());
            }
        }
        None
    }
}
