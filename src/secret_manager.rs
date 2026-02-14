#[allow(unused_imports)]
pub use crate::keychain_ffi::{
    delete_generic_password, find_generic_password, find_generic_password_with_context,
    set_generic_password, set_generic_password_with_biometric_protection, KeychainError,
};
use crate::biometric_auth::AuthContext;
use crate::keychain_ffi;
use crate::secret_types;
use std::collections::HashMap;

// Re-export shared constants for API compatibility
pub use crate::secret_types::{KNOWN_KEYS, composio_key_name};

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

use crate::secret_types::SecretManagerTrait;

impl SecretManager {
    /// Create a new empty SecretManager
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }

    /// Get a cloned secret value
    #[allow(dead_code)]
    pub fn get_cloned(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
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

        // Load custom tool credentials from the index (BYOA)
        match keychain_ffi::find_generic_password_with_context("composio_custom_keys_index", context) {
            Ok(index_csv) => {
                tracing::info!("Found custom tools index, loading credentials with biometric context...");
                for key in index_csv.split(',') {
                    let key = key.trim();
                    if !key.is_empty() {
                        match keychain_ffi::find_generic_password_with_context(key, context) {
                            Ok(value) => {
                                self.secrets.insert(key.to_string(), value);
                                tracing::debug!("Loaded custom tool secret with context: {}", key);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to load indexed secret '{}' with context: {}", key, e);
                            }
                        }
                    }
                }
            }
            Err(_) => {
                tracing::debug!("No custom tools index found (composio_custom_keys_index).");
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
    /// * `profile_id` - The stable ID of the Composio profile
    /// * `context` - Optional authenticated AuthContext; falls back to regular access if None
    pub fn load_composio_key_with_context(
        &mut self,
        profile_id: &str,
        context: Option<&AuthContext>,
    ) {
        let key = composio_key_name(profile_id);

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
                tracing::debug!("Loaded Composio key for profile id: {}", profile_id);
            }
            Err(keychain_ffi::KeychainError::NotFound) => {
                tracing::debug!("No Composio key found for profile id: {}", profile_id);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load Composio key for profile id '{}': {}",
                    profile_id,
                    e
                );
            }
        }
    }
}

impl SecretManagerTrait for SecretManager {
    /// Load all known secrets from the Keychain in a single session.
    /// Uses our custom FFI that includes the proper access group for sandboxed apps.
    fn load_all_from_keychain(&mut self) {
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
                    tracing::debug!("Failed to load secret '{}': {}", key, e);
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
                tracing::debug!("Failed to load legacy composio_api_key: {}", e);
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

    /// Get a secret by key
    fn get(&self, key: &str) -> Option<&String> {
        self.secrets.get(key)
    }

    /// Set a secret (updates cache and saves to keychain with biometric protection)
    /// Falls back to regular keychain save if biometric protection fails (e.g., missing entitlements)
    fn set(&mut self, key: &str, value: String) -> Result<(), String> {
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
    fn delete(&mut self, key: &str) -> Result<(), String> {
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

    /// Load a Composio profile key from keychain (for dynamically discovered profiles)
    fn load_composio_key(&mut self, profile_id: &str) {
        let key = composio_key_name(profile_id);

        // Use our FFI to load (includes access group)
        match keychain_ffi::find_generic_password(&key) {
            Ok(value) => {
                self.secrets.insert(key, value);
                tracing::debug!("Loaded Composio key for profile id: {}", profile_id);
            }
            Err(keychain_ffi::KeychainError::NotFound) => {}
            Err(e) => {
                tracing::warn!("Failed to load Composio key for profile id '{}': {}", profile_id, e);
            }
        }
    }

    /// Update the internal cache without performing any keychain operations.
    fn update_cache(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }

    /// Delete all cached secrets from keychain.
    fn delete_all(&mut self) -> Vec<String> {
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

    /// Get the current index value directly from keychain (for index updates).
    fn get_index_from_keychain(&self) -> Option<String> {
        keychain_ffi::find_generic_password(secret_types::CUSTOM_KEYS_INDEX_KEY).ok()
    }

    /// Get a reference to the secrets cache for credential extraction.
    fn secrets_ref(&self) -> &HashMap<String, String> {
        &self.secrets
    }
}
