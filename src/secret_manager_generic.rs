#![cfg(not(target_os = "macos"))]

use std::collections::HashMap;
use keyring::Entry;
use crate::constants::SERVICE_NAME;

/// Known secret keys used by the application
pub const KNOWN_KEYS: &[&str] = &[
    "api_key",          // Gemini API key
    "smithery_api_key", // Smithery API key
];

/// Prefix for Composio profile API keys
pub const COMPOSIO_KEY_PREFIX: &str = "composio_api_key_";

/// Dummy AuthContext for non-macOS generic implementation.
/// This matches the type in the macOS implementation for API parity.
#[derive(Clone, Debug)]
pub struct AuthContext;

/// Error types for keychain operations (API parity with macOS)
#[derive(Debug, Clone)]
pub enum KeychainError {
    NotFound,
    AuthCancelled,
    AuthRequired,
    DecodingError(String),
    SecurityError(i32),
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeychainError::NotFound => write!(f, "Item not found in keychain"),
            KeychainError::AuthCancelled => write!(f, "Authentication was cancelled"),
            KeychainError::AuthRequired => write!(f, "Authentication is required"),
            KeychainError::DecodingError(msg) => write!(f, "Failed to decode: {}", msg),
            KeychainError::SecurityError(code) => write!(f, "Security error: {}", code),
        }
    }
}

impl std::error::Error for KeychainError {}

/// Stub for set_generic_password_with_biometric_protection (API parity)
pub fn set_generic_password_with_biometric_protection(
    account: &str,
    password: &str,
) -> Result<(), KeychainError> {
    set_generic_password(account, password)
}

/// Stub for set_generic_password (API parity)
pub fn set_generic_password(account: &str, password: &str) -> Result<(), KeychainError> {
    if let Ok(entry) = Entry::new(SERVICE_NAME, account) {
        entry.set_password(password).map_err(|_| KeychainError::SecurityError(-1))?;
        Ok(())
    } else {
        Err(KeychainError::SecurityError(-1))
    }
}

/// Centralized secret manager that caches secrets in memory
/// and provides efficient batch loading from platform-native keychains.
#[derive(Clone, Debug)]
pub struct SecretManager {
    secrets: HashMap<String, String>,
}

impl Default for SecretManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretManager {
    /// Create a new empty SecretManager
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }

    /// Load all known secrets from the platform keychain.
    pub fn load_all_from_keychain(&mut self) {
        // Load known static keys
        for key in KNOWN_KEYS {
            if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
                match entry.get_password() {
                    Ok(value) => {
                        self.secrets.insert(key.to_string(), value);
                        tracing::debug!("Loaded secret: {}", key);
                    }
                    Err(keyring::Error::NoEntry) => {
                        tracing::debug!("Secret not found: {}", key);
                    }
                    Err(e) => {
                        tracing::debug!("Failed to load secret '{}': {}", key, e);
                    }
                }
            }
        }

        // Load legacy composio_api_key if it exists (for migration)
        if let Ok(entry) = Entry::new(SERVICE_NAME, "composio_api_key") {
            if let Ok(value) = entry.get_password() {
                self.secrets.insert("composio_api_key".to_string(), value);
                tracing::debug!("Loaded legacy composio_api_key");
            }
        }

        // Feature: Dynamic Tool Credentials via Index Key
        if let Ok(entry) = Entry::new(SERVICE_NAME, "composio_custom_keys_index") {
            if let Ok(index_csv) = entry.get_password() {
                tracing::info!("Found custom tools index, loading credentials...");
                for key in index_csv.split(',') {
                    let key = key.trim();
                    if !key.is_empty() {
                         if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
                            match entry.get_password() {
                                Ok(value) => {
                                    self.secrets.insert(key.to_string(), value);
                                    tracing::debug!("Loaded custom tool secret: {}", key);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to load indexed secret '{}': {}", key, e);
                                }
                            }
                        }
                    }
                }
            }
        }

        tracing::info!(
            "SecretManager (Generic) loaded {} secrets from keychain",
            self.secrets.len()
        );
    }

    /// Retrieve all loaded custom tool credentials formatted as:
    /// Map<ToolkitSlug, Map<FieldName, Value>>
    pub fn get_custom_tool_credentials(&self) -> HashMap<String, HashMap<String, String>> {
        let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();
        let prefix = "composio_tool_";
        let separator = "__";

        for (key, value) in &self.secrets {
            if let Some(rest) = key.strip_prefix(prefix) {
                if let Some((slug, field)) = rest.split_once(separator) {
                    if !slug.is_empty() && !field.is_empty() {
                        result
                            .entry(slug.to_string())
                            .or_default()
                            .insert(field.to_string(), value.clone());
                    }
                } else {
                    tracing::warn!("Ignored malformed custom tool key: {}", key);
                }
            }
        }
        result
    }

    /// Check if there are any custom credentials for a specific toolkit slug
    pub fn has_custom_tool_credentials(&self, slug: &str) -> bool {
        let prefix = format!("composio_tool_{}__", slug);
        self.secrets.keys().any(|k| k.starts_with(&prefix))
    }

    /// Set a custom tool credential and update the index
    pub fn set_custom_tool_credential(&mut self, slug: &str, field: &str, value: String) -> Result<(), String> {
        let key = format!("composio_tool_{}__{}", slug, field);
        
        // 1. Save the actual secret
        self.set(&key, value.clone())?;

        // 2. Update Index
        let index_key = "composio_custom_keys_index";
        let current_index = self.get_from_keychain_directly(index_key).unwrap_or_default();

        let key_exists = current_index.split(',').any(|k| k.trim() == key);
        
        if !key_exists {
            let new_index = if current_index.is_empty() {
                key.clone()
            } else {
                format!("{},{}", current_index, key)
            };
            
             self.set(index_key, new_index)?;
             tracing::info!("Updated custom tool index with new key: {}", key);
        }

        Ok(())
    }

    /// Delete a custom tool credential and update the index
    pub fn delete_custom_tool_credential(&mut self, slug: &str, field: &str) -> Result<(), String> {
        let key = format!("composio_tool_{}__{}", slug, field);
        
        // 1. Delete the actual secret
        let _ = self.delete(&key);

        // 2. Update Index
        let index_key = "composio_custom_keys_index";
        let current_index = self.get_from_keychain_directly(index_key).unwrap_or_default();

        if !current_index.is_empty() {
              let new_index_parts: Vec<&str> = current_index
                .split(',')
                .map(|k| k.trim())
                .filter(|k| *k != key && !k.is_empty())
                .collect();
            
            let new_index = new_index_parts.join(",");
            
             self.set(index_key, new_index)?;
             tracing::info!("Removed custom tool key from index: {}", key);
        }

        Ok(())
    }

    /// Internal helper to pull directly from platform keychain without caching
    fn get_from_keychain_directly(&self, key: &str) -> Option<String> {
        if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
            entry.get_password().ok()
        } else {
            None
        }
    }

    /// Get a secret by key
    pub fn get(&self, key: &str) -> Option<&String> {
        self.secrets.get(key)
    }

    /// Get a cloned secret value
    #[allow(dead_code)]
    pub fn get_cloned(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }

    /// Set a secret (updates cache and saves to keychain)
    #[allow(dead_code)]
    pub fn set(&mut self, key: &str, value: String) -> Result<(), String> {
        let entry = Entry::new(SERVICE_NAME, key)
            .map_err(|e| format!("Failed to create keyring entry: {}", e))?;
            
        entry.set_password(&value)
            .map_err(|e| format!("Failed to save secret to Keyring: {}", e))?;
            
        self.secrets.insert(key.to_string(), value);
        tracing::debug!("Saved secret: {}", key);
        Ok(())
    }

    /// Delete a secret (removes from cache and keychain)
    #[allow(dead_code)]
    pub fn delete(&mut self, key: &str) -> Result<(), String> {
        if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
            let _ = entry.delete_password();
        }
        self.secrets.remove(key);
        tracing::debug!("Deleted secret: {}", key);
        Ok(())
    }

    pub fn get_composio_key(&self, profile_name: &str) -> Option<&String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.secrets.get(&key)
    }

    #[allow(dead_code)]
    pub fn set_composio_key(&mut self, profile_name: &str, value: String) -> Result<(), String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.set(&key, value)
    }

    #[allow(dead_code)]
    pub fn delete_composio_key(&mut self, profile_name: &str) -> Result<(), String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.delete(&key)
    }

    pub fn load_composio_key(&mut self, profile_name: &str) {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        if let Some(value) = self.get_from_keychain_directly(&key) {
             self.secrets.insert(key, value);
             tracing::debug!("Loaded Composio key for profile: {}", profile_name);
        }
    }

    // =========================================================================
    // BIOMETRIC AUTHENTICATION STUBS (For API Parity with macOS)
    // =========================================================================

    pub fn load_all_with_context(&mut self, _context: &AuthContext) {
        tracing::debug!("Generic SecretManager: Ignoring biometric context, loading normally...");
        self.load_all_from_keychain();
    }

    pub fn load_composio_key_with_context(
        &mut self,
        profile_name: &str,
        _context: Option<&AuthContext>,
    ) {
        self.load_composio_key(profile_name);
    }

    pub fn update_cache(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }

    pub fn delete_all(&mut self) -> Vec<String> {
        let keys: Vec<String> = self.secrets.keys().cloned().collect();
        let mut deleted = Vec::new();

        for key in &keys {
            if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
                if entry.delete_password().is_ok() {
                    deleted.push(key.clone());
                }
            }
        }

        self.secrets.clear();
        deleted
    }
}
