#![cfg(target_os = "macos")]

use crate::biometric_auth::AuthContext;
use crate::keychain_ffi;
use std::collections::HashMap;

/// Known secret keys used by the application
pub const KNOWN_KEYS: &[&str] = &[
    "api_key",          // Gemini API key
    "smithery_api_key", // Smithery API key
];

/// Prefix for Composio profile API keys
pub const COMPOSIO_KEY_PREFIX: &str = "composio_api_key_";

/// Centralized secret manager that caches secrets in memory
/// and provides efficient batch loading from the macOS Keychain.
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

    /// Load all known secrets from the Keychain in a single session.
    /// Uses our custom FFI that includes the proper access group for sandboxed apps.
    pub fn load_all_from_keychain(&mut self) {
        // Load known static keys using our FFI (includes access group)
        for key in KNOWN_KEYS {
            match keychain_ffi::find_generic_password(key) {
                Ok(value) => {
                    self.secrets.insert(key.to_string(), value);
                    tracing::debug!("Loaded secret: {}", key);
                }
                Err(keychain_ffi::KeychainError::NotFound) => {
                    tracing::debug!("Secret not found: {}", key);
                }
                Err(e) => {
                    tracing::warn!("Failed to load secret '{}': {}", key, e);
                }
            }
        }

        // Load legacy composio_api_key if it exists (for migration)
        match keychain_ffi::find_generic_password("composio_api_key") {
            Ok(value) => {
                self.secrets.insert("composio_api_key".to_string(), value);
                tracing::debug!("Loaded legacy composio_api_key");
            }
            Err(keychain_ffi::KeychainError::NotFound) => {}
            Err(e) => {
                tracing::warn!("Failed to load legacy composio_api_key: {}", e);
            }
        }

        // Feature: Dynamic Tool Credentials via Index Key
        // We maintain a comma-separated list of custom keys in `composio_custom_keys_index`.
        match keychain_ffi::find_generic_password("composio_custom_keys_index") {
            Ok(index_csv) => {
                tracing::info!("Found custom tools index, loading credentials...");
                for key in index_csv.split(',') {
                    let key = key.trim();
                    if !key.is_empty() {
                         match keychain_ffi::find_generic_password(key) {
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
            Err(_) => {
                tracing::debug!("No custom tools index found (composio_custom_keys_index).");
            }
        }

        tracing::info!(
            "SecretManager loaded {} secrets from keychain",
            self.secrets.len()
        );
    }

    /// Retrieve all loaded custom tool credentials formatted as:
    /// Map<ToolkitSlug, Map<FieldName, Value>>
    /// Keys are expected to be in format: `composio_tool_{slug}__{field}`
    pub fn get_custom_tool_credentials(&self) -> HashMap<String, HashMap<String, String>> {
        let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();
        let prefix = "composio_tool_";
        let separator = "__";

        for (key, value) in &self.secrets {
            if let Some(rest) = key.strip_prefix(prefix) {
                // Split by separator
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

    /// Set a custom tool credential and update the index
    pub fn set_custom_tool_credential(&mut self, slug: &str, field: &str, value: String) -> Result<(), String> {
        let key = format!("composio_tool_{}__{}", slug, field);
        
        // 1. Save the actual secret
        self.set(&key, value.clone())?;

        // 2. Update Index
        let index_key = "composio_custom_keys_index";
        let current_index = keychain_ffi::find_generic_password(index_key).unwrap_or_default();

        // Check if key exists in index
        let key_exists = current_index.split(',').any(|k| k.trim() == key);
        
        if !key_exists {
            let new_index = if current_index.is_empty() {
                key.clone()
            } else {
                format!("{},{}", current_index, key)
            };
            
            // Save new index
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
        let current_index = keychain_ffi::find_generic_password(index_key).unwrap_or_default();

        if !current_index.is_empty() {
             let new_index_parts: Vec<&str> = current_index
                .split(',')
                .map(|k| k.trim())
                .filter(|k| *k != key && !k.is_empty())
                .collect();
            
            let new_index = new_index_parts.join(",");
            
            // Save new index
             self.set(index_key, new_index)?;
             tracing::info!("Removed custom tool key from index: {}", key);
        }

        Ok(())
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

    /// Set a secret (updates cache and saves to keychain with biometric protection)
    /// Falls back to regular keychain save if biometric protection fails (e.g., missing entitlements)
    #[allow(dead_code)]
    pub fn set(&mut self, key: &str, value: String) -> Result<(), String> {
        // Try to save with biometric protection first
        match keychain_ffi::set_generic_password_with_biometric_protection(key, &value) {
            Ok(()) => {
                self.secrets.insert(key.to_string(), value);
                tracing::debug!("Saved secret with biometric protection: {}", key);
                Ok(())
            }
            Err(keychain_ffi::KeychainError::SecurityError(-34018)) => {
                // -34018 = errSecMissingEntitlement - app not properly signed
                // Fallback to regular keychain save without biometric protection
                tracing::warn!(
                    "Biometric protection unavailable (missing entitlement), falling back to regular keychain save for: {}",
                    key
                );
                keychain_ffi::set_generic_password(key, &value)
                    .map_err(|e| format!("Failed to save secret to Keychain: {}", e))?;
                self.secrets.insert(key.to_string(), value);
                tracing::debug!("Saved secret (without biometric protection): {}", key);
                Ok(())
            }
            Err(e) => Err(format!("Failed to save secret to Keychain: {}", e)),
        }
    }

    /// Delete a secret (removes from cache and keychain)
    #[allow(dead_code)]
    pub fn delete(&mut self, key: &str) -> Result<(), String> {
        // Use our FFI to delete (includes access group)
        match keychain_ffi::delete_generic_password(key) {
            Ok(()) => {
                self.secrets.remove(key);
                tracing::debug!("Deleted secret: {}", key);
                Ok(())
            }
            Err(keychain_ffi::KeychainError::NotFound) => {
                // Already doesn't exist, just remove from cache
                self.secrets.remove(key);
                Ok(())
            }
            Err(e) => Err(format!("Failed to delete secret: {}", e)),
        }
    }

    /// Get the Composio API key for a specific profile
    pub fn get_composio_key(&self, profile_name: &str) -> Option<&String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.secrets.get(&key)
    }

    /// Set the Composio API key for a specific profile
    #[allow(dead_code)]
    pub fn set_composio_key(&mut self, profile_name: &str, value: String) -> Result<(), String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.set(&key, value)
    }

    /// Delete the Composio API key for a specific profile
    #[allow(dead_code)]
    pub fn delete_composio_key(&mut self, profile_name: &str) -> Result<(), String> {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        self.delete(&key)
    }

    /// Load a Composio profile key from keychain (for dynamically discovered profiles)
    pub fn load_composio_key(&mut self, profile_name: &str) {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);

        // Use our FFI to load (includes access group)
        match keychain_ffi::find_generic_password(&key) {
            Ok(value) => {
                self.secrets.insert(key, value);
                tracing::debug!("Loaded Composio key for profile: {}", profile_name);
            }
            Err(keychain_ffi::KeychainError::NotFound) => {}
            Err(e) => {
                tracing::warn!("Failed to load Composio key for '{}': {}", profile_name, e);
            }
        }
    }

    // =========================================================================
    // BIOMETRIC AUTHENTICATION METHODS
    // These methods use an authenticated LAContext to avoid repeated prompts
    // =========================================================================

    /// Load all known secrets using a pre-authenticated biometric context.
    ///
    /// This is the preferred method for loading secrets as it uses the
    /// authenticated LAContext to avoid prompting the user multiple times.
    ///
    /// # Arguments
    /// * `context` - An authenticated AuthContext from biometric authentication
    pub fn load_all_with_context(&mut self, context: &AuthContext) {
        tracing::debug!("Loading secrets with biometric context...");

        // Load known static keys
        for key in KNOWN_KEYS {
            match keychain_ffi::find_generic_password_with_context(key, context) {
                Ok(value) => {
                    self.secrets.insert(key.to_string(), value);
                    tracing::debug!("Loaded secret with context: {}", key);
                }
                Err(keychain_ffi::KeychainError::NotFound) => {
                    tracing::debug!("Secret not found: {}", key);
                }
                Err(e) => {
                    tracing::warn!("Failed to load secret '{}' with context: {}", key, e);
                }
            }
        }

        // Load legacy composio_api_key if it exists (for migration)
        match keychain_ffi::find_generic_password_with_context("composio_api_key", context) {
            Ok(value) => {
                self.secrets.insert("composio_api_key".to_string(), value);
                tracing::debug!("Loaded legacy composio_api_key with context");
            }
            Err(keychain_ffi::KeychainError::NotFound) => {}
            Err(e) => {
                tracing::warn!("Failed to load legacy composio_api_key with context: {}", e);
            }
        }

        tracing::debug!(
            "SecretManager loaded {} secrets with biometric context",
            self.secrets.len()
        );
    }

    /// Load a Composio profile key using a pre-authenticated biometric context.
    ///
    /// # Arguments
    /// * `profile_name` - The name of the Composio profile
    /// * `context` - Optional authenticated AuthContext; falls back to regular access if None
    pub fn load_composio_key_with_context(
        &mut self,
        profile_name: &str,
        context: Option<&AuthContext>,
    ) {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);

        let result = if let Some(ctx) = context {
            match keychain_ffi::find_generic_password_with_context(&key, ctx) {
                Ok(val) => Ok(val),
                Err(keychain_ffi::KeychainError::NotFound) => {
                    // Fallback: Try loading without context (in case it was saved without biometric protection)
                    tracing::debug!(
                        "Key '{}' not found with context, trying fallback lookup",
                        key
                    );
                    keychain_ffi::find_generic_password(&key)
                }
                Err(e) => Err(e),
            }
        } else {
            keychain_ffi::find_generic_password(&key)
        };

        match result {
            Ok(value) => {
                self.secrets.insert(key, value);
                tracing::debug!("Loaded Composio key for profile: {}", profile_name);
            }
            Err(keychain_ffi::KeychainError::NotFound) => {
                tracing::debug!("No Composio key found for profile: {}", profile_name);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load Composio key for profile '{}': {}",
                    profile_name,
                    e
                );
            }
        }
    }
    /// Update the internal cache without performing any keychain operations.
    /// Useful when the keychain has been updated via a background task.
    pub fn update_cache(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }

    /// Delete all cached secrets from keychain.
    /// This is useful for resetting keychain items so they can be re-saved
    /// with biometric protection (when upgrading from non-biometric items).
    ///
    /// Returns the list of keys that were deleted.
    pub fn delete_all(&mut self) -> Vec<String> {
        let keys: Vec<String> = self.secrets.keys().cloned().collect();
        let mut deleted = Vec::new();

        for key in &keys {
            match keychain_ffi::delete_generic_password(key) {
                Ok(()) => {
                    tracing::info!("Deleted keychain item: {}", key);
                    deleted.push(key.clone());
                }
                Err(keychain_ffi::KeychainError::NotFound) => {
                    tracing::debug!("Keychain item already gone: {}", key);
                    deleted.push(key.clone());
                }
                Err(e) => {
                    tracing::error!("Failed to delete keychain item '{}': {}", key, e);
                }
            }
        }

        // Clear the cache
        self.secrets.clear();

        tracing::info!("Cleared {} secrets from keychain and cache", deleted.len());
        deleted
    }
}


