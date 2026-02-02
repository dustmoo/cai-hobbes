#![cfg(not(target_os = "macos"))]

use std::collections::HashMap;
use keyring::Entry;
use crate::constants::SERVICE_NAME;
use crate::secret_types;

// Re-export shared constants for API compatibility
pub use crate::secret_types::{KNOWN_KEYS, COMPOSIO_KEY_PREFIX};

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

    /// Internal helper to pull directly from platform keychain without caching
    fn get_from_keychain_directly(&self, key: &str) -> Option<String> {
        if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
            entry.get_password().ok()
        } else {
            None
        }
    }
}

impl SecretManagerTrait for SecretManager {
    /// Load all known secrets from the platform keychain.
    fn load_all_from_keychain(&mut self) {
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

    /// Get a secret by key
    fn get(&self, key: &str) -> Option<&String> {
        self.secrets.get(key)
    }

    /// Set a secret (updates cache and saves to keychain)
    fn set(&mut self, key: &str, value: String) -> Result<(), String> {
        let entry = Entry::new(SERVICE_NAME, key)
            .map_err(|e| format!("Failed to create keyring entry: {}", e))?;
            
        entry.set_password(&value)
            .map_err(|e| format!("Failed to save secret to Keyring: {}", e))?;
            
        self.secrets.insert(key.to_string(), value);
        tracing::debug!("Saved secret: {}", key);
        Ok(())
    }

    /// Delete a secret (removes from cache and keychain)
    fn delete(&mut self, key: &str) -> Result<(), String> {
        if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
            let _ = entry.delete_password();
        }
        self.secrets.remove(key);
        tracing::debug!("Deleted secret: {}", key);
        Ok(())
    }

    fn load_composio_key(&mut self, profile_name: &str) {
        let key = format!("{}{}", COMPOSIO_KEY_PREFIX, profile_name);
        if let Some(value) = self.get_from_keychain_directly(&key) {
             self.secrets.insert(key, value);
             tracing::debug!("Loaded Composio key for profile: {}", profile_name);
        }
    }

    fn update_cache(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }

    fn delete_all(&mut self) -> Vec<String> {
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

    /// Get the current index value directly from keychain (for index updates).
    fn get_index_from_keychain(&self) -> Option<String> {
        self.get_from_keychain_directly(secret_types::CUSTOM_KEYS_INDEX_KEY)
    }

    /// Get a reference to the secrets cache for credential extraction.
    fn secrets_ref(&self) -> &HashMap<String, String> {
        &self.secrets
    }
}
