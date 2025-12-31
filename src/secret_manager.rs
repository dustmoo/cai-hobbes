#![cfg(target_os = "macos")]

use crate::biometric_auth::AuthContext;
use crate::keychain_ffi;
use security_framework::os::macos::keychain::SecKeychain;
use std::collections::HashMap;

use crate::constants::SERVICE_NAME;

/// Known secret keys used by the application
pub const KNOWN_KEYS: &[&str] = &[
    "api_key",           // Gemini API key
    "smithery_api_key",  // Smithery API key
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
    /// Also discovers and loads any Composio profile keys.
    pub fn load_all_from_keychain(&mut self) {
        let keychain = match SecKeychain::default() {
            Ok(kc) => kc,
            Err(e) => {
                tracing::error!("Failed to open keychain: {}", e);
                return;
            }
        };

        // Load known static keys
        for key in KNOWN_KEYS {
            if let Ok((password, _)) = keychain.find_generic_password(SERVICE_NAME, key) {
                if let Ok(value) = String::from_utf8(password.to_vec()) {
                    self.secrets.insert(key.to_string(), value);
                    tracing::debug!("Loaded secret: {}", key);
                }
            }
        }

        // Load legacy composio_api_key if it exists (for migration)
        if let Ok((password, _)) = keychain.find_generic_password(SERVICE_NAME, "composio_api_key") {
            if let Ok(value) = String::from_utf8(password.to_vec()) {
                self.secrets.insert("composio_api_key".to_string(), value);
                tracing::debug!("Loaded legacy composio_api_key");
            }
        }

        tracing::debug!("SecretManager loaded {} secrets from keychain", self.secrets.len());
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
        let keychain = SecKeychain::default().map_err(|e| e.to_string())?;
        
        if let Ok((_, item)) = keychain.find_generic_password(SERVICE_NAME, key) {
            item.delete();
        }

        self.secrets.remove(key);
        tracing::debug!("Deleted secret: {}", key);
        Ok(())
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
        let keychain = match SecKeychain::default() {
            Ok(kc) => kc,
            Err(_) => return,
        };

        if let Ok((password, _)) = keychain.find_generic_password(SERVICE_NAME, &key) {
            if let Ok(value) = String::from_utf8(password.to_vec()) {
                self.secrets.insert(key, value);
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

        tracing::debug!("SecretManager loaded {} secrets with biometric context", self.secrets.len());
    }

    /// Load a Composio profile key using a pre-authenticated biometric context.
    /// 
    /// # Arguments
    /// * `profile_name` - The name of the Composio profile
    /// * `context` - Optional authenticated AuthContext; falls back to regular access if None
    pub fn load_composio_key_with_context(&mut self, profile_name: &str, context: Option<&AuthContext>) {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        
        let result = if let Some(ctx) = context {
            match keychain_ffi::find_generic_password_with_context(&key, ctx) {
                Ok(val) => Ok(val),
                Err(keychain_ffi::KeychainError::NotFound) => {
                     // Fallback: Try loading without context (in case it was saved without biometric protection)
                     tracing::debug!("Key '{}' not found with context, trying fallback lookup", key);
                     keychain_ffi::find_generic_password(&key)
                },
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
                tracing::warn!("Failed to load Composio key for profile '{}': {}", profile_name, e);
            }
        }
    }
    /// Update the internal cache without performing any keychain operations.
    /// Useful when the keychain has been updated via a background task.
    pub fn update_cache(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }
}
