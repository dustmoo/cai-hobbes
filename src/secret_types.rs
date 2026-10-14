//! Shared types and helpers for secret management across platforms.
//!
//! This module is unconditionally compiled and contains no platform-specific code.

use std::collections::HashMap;

// ============================================================================
// CONSTANTS
// ============================================================================

/// Known secret keys used by the application
pub const KNOWN_KEYS: &[&str] = &[
    "api_key",               // Legacy Gemini API key (migrated to gemini_api_key)
    "gemini_api_key",        // Gemini API key (provider-scoped)
    "claude_api_key",        // Claude API key (provider-scoped)
    "openai_compat_api_key", // OpenAI-compatible API key (provider-scoped)
    "smithery_api_key",      // Smithery API key
    "hobbes_license",        // Hobbes Pro license key (entitlement::LICENSE_KEYCHAIN_KEY)
];

/// Prefix for Composio profile API keys (e.g., "composio_api_key_default")
pub const COMPOSIO_KEY_PREFIX: &str = "composio_api_key_";

/// Build the full keychain key for a Composio profile's API key.
pub fn composio_key_name(profile_id: &str) -> String {
    format!("{}{}", COMPOSIO_KEY_PREFIX, profile_id)
}

/// Prefix for per-connector LLM API keys (e.g., "llm_api_key_<uuid>")
pub const LLM_KEY_PREFIX: &str = "llm_api_key_";

/// Build the full keychain key for an LLM connector instance's API key.
pub fn llm_key_name(connector_id: &str) -> String {
    format!("{}{}", LLM_KEY_PREFIX, connector_id)
}

/// Key name for the CSV index of all per-connector LLM API keys.
/// Needed so `load_all_from_keychain` can discover dynamically-named keys.
pub const LLM_KEYS_INDEX_KEY: &str = "llm_connector_keys_index";

/// Prefix for per-subscription calendar feed URLs (e.g., "cal_url_<uuid>").
/// ICS "secret address" URLs embed access tokens, so they are secrets and
/// live in the keychain — settings holds only the subscription id.
pub const CAL_URL_PREFIX: &str = "cal_url_";

/// Build the full keychain key for a calendar subscription's feed URL.
pub fn cal_url_key_name(subscription_id: &str) -> String {
    format!("{}{}", CAL_URL_PREFIX, subscription_id)
}

/// Key name for the CSV index of all calendar feed URL keys.
/// Needed so `load_all_from_keychain` can discover dynamically-named keys.
pub const CAL_KEYS_INDEX_KEY: &str = "cal_keys_index";

/// Prefix for custom tool credentials (e.g., "composio_tool_slack__api_key")
pub const CUSTOM_TOOL_PREFIX: &str = "composio_tool_";

/// Separator between slug and field in custom tool keys
pub const CUSTOM_TOOL_SEPARATOR: &str = "__";

/// Key name for the CSV index of all custom tool keys
pub const CUSTOM_KEYS_INDEX_KEY: &str = "composio_custom_keys_index";

// ============================================================================
// CUSTOM TOOL KEY HELPERS
// ============================================================================

/// Format a custom tool credential key from its components, optionally scoped to a profile.
///
/// # Example
/// ```
/// assert_eq!(format_custom_tool_key(Some("Puget"), "slack", "api_key"), "composio_tool_p:Puget:slack__api_key");
/// assert_eq!(format_custom_tool_key(None, "slack", "api_key"), "composio_tool_slack__api_key");
/// ```
pub fn format_custom_tool_key(profile: Option<&str>, slug: &str, field: &str) -> String {
    match profile {
        Some(p) => format!(
            "{}p:{}:{}{}{}",
            CUSTOM_TOOL_PREFIX, p, slug, CUSTOM_TOOL_SEPARATOR, field
        ),
        None => format!(
            "{}{}{}{}",
            CUSTOM_TOOL_PREFIX, slug, CUSTOM_TOOL_SEPARATOR, field
        ),
    }
}

/// Parse a custom tool credential key into its optional profile, slug and field components.
///
/// Returns `None` if the key doesn't match the expected format.
pub fn parse_custom_tool_key(key: &str) -> Option<(Option<String>, String, String)> {
    let rest = key.strip_prefix(CUSTOM_TOOL_PREFIX)?;

    // Check for profile scoping: p:{profile}:{slug}__field
    if let Some(p_rest) = rest.strip_prefix("p:") {
        if let Some((profile_part, slug_field_part)) = p_rest.split_once(':') {
            let (slug, field) = slug_field_part.split_once(CUSTOM_TOOL_SEPARATOR)?;
            if slug.is_empty() || field.is_empty() || profile_part.is_empty() {
                return None;
            }
            return Some((
                Some(profile_part.to_string()),
                slug.to_string(),
                field.to_string(),
            ));
        }
    }

    // Legacy/Global fallback: {slug}__field
    let (slug, field) = rest.split_once(CUSTOM_TOOL_SEPARATOR)?;
    if slug.is_empty() || field.is_empty() {
        return None;
    }
    Some((None, slug.to_string(), field.to_string()))
}

/// Check if a given key belongs to a specific toolkit slug (ignoring profile).
pub fn key_belongs_to_toolkit(key: &str, slug: &str) -> bool {
    if let Some((_, k_slug, _)) = parse_custom_tool_key(key) {
        k_slug == slug
    } else {
        false
    }
}

// ============================================================================
// INDEX MANIPULATION HELPERS
// ============================================================================

/// Parse the comma-separated index string into a list of keys.
pub fn parse_index_csv(csv_string: &str) -> Vec<&str> {
    csv_string
        .split(',')
        .map(|k| k.trim())
        .filter(|k| !k.is_empty())
        .collect()
}

/// Add a key to the index CSV string if it doesn't already exist.
///
/// Returns the new index string.
pub fn add_to_index_csv(current_index: &str, new_key: &str) -> String {
    let keys = parse_index_csv(current_index);
    if keys.contains(&new_key) {
        return current_index.to_string();
    }
    if current_index.is_empty() {
        new_key.to_string()
    } else {
        format!("{},{}", current_index, new_key)
    }
}

/// Remove a key from the index CSV string.
///
/// Returns the new index string.
pub fn remove_from_index_csv(current_index: &str, key_to_remove: &str) -> String {
    parse_index_csv(current_index)
        .into_iter()
        .filter(|k| *k != key_to_remove)
        .collect::<Vec<_>>()
        .join(",")
}

// ============================================================================
// TRAIT DEFINITION
// ============================================================================

/// Common trait for secret management across platforms.
///
/// This trait formalizes the API parity between macOS and generic implementations.
/// Methods with default implementations reduce duplication; platform-specific
/// implementations only need to provide the core keychain operations.
pub trait SecretManagerTrait {
    // -------------------------------------------------------------------------
    // REQUIRED METHODS (platform-specific)
    // -------------------------------------------------------------------------

    /// Load all known secrets from the platform keychain.
    fn load_all_from_keychain(&mut self);

    /// Get a secret by key from the in-memory cache.
    fn get(&self, key: &str) -> Option<&String>;

    /// Set a secret (updates cache and saves to keychain).
    fn set(&mut self, key: &str, value: String) -> Result<(), String>;

    /// Set a secret with an explicit protection level. Platforms without
    /// biometric ACLs ignore the flag; the macOS implementation overrides
    /// this so `biometric: false` writes a plain keychain item that stays
    /// readable without an authentication prompt.
    fn set_with_protection(
        &mut self,
        key: &str,
        value: String,
        _biometric: bool,
    ) -> Result<(), String> {
        self.set(key, value)
    }

    /// Delete a secret (removes from cache and keychain).
    fn delete(&mut self, key: &str) -> Result<(), String>;

    /// Load a specific Composio key into the cache from keychain.
    fn load_composio_key(&mut self, profile_id: &str);

    /// Load a specific LLM connector key into the cache from keychain.
    fn load_llm_key(&mut self, connector_id: &str);

    /// Manually update a secret in the cache without keychain write.
    fn update_cache(&mut self, key: String, value: String);

    /// Delete all loaded secrets from the platform keychain.
    fn delete_all(&mut self) -> Vec<String>;

    /// Get a named CSV-index value directly from keychain (for index updates).
    fn get_named_index_from_keychain(&self, index_key: &str) -> Option<String>;

    /// Get the custom-tool index value directly from keychain.
    fn get_index_from_keychain(&self) -> Option<String> {
        self.get_named_index_from_keychain(CUSTOM_KEYS_INDEX_KEY)
    }

    /// Get a reference to the secrets cache for credential extraction.
    fn secrets_ref(&self) -> &std::collections::HashMap<String, String>;

    /// Check if a key exists in the secrets cache.
    fn has_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    // -------------------------------------------------------------------------
    // DEFAULT IMPLEMENTATIONS (shared logic)
    // -------------------------------------------------------------------------

    /// Retrieve all loaded custom tool credentials for all profiles.
    fn get_all_custom_tool_credentials(&self) -> HashMap<String, HashMap<String, String>> {
        extract_custom_tool_credentials(None, self.secrets_ref())
    }

    /// Retrieve all loaded custom tool credentials for a specific profile.
    ///
    /// Returns a nested map: `Map<ToolkitSlug, Map<FieldName, Value>>`.
    fn get_custom_tool_credentials(
        &self,
        profile_name: Option<&str>,
    ) -> HashMap<String, HashMap<String, String>> {
        extract_custom_tool_credentials(profile_name, self.secrets_ref())
    }

    /// Check if there are any custom credentials for a specific toolkit slug in any profile.
    fn has_custom_tool_credentials(&self, slug: &str) -> bool {
        self.secrets_ref()
            .keys()
            .any(|k| key_belongs_to_toolkit(k, slug))
    }

    /// Set a custom tool credential and update the index. `biometric`
    /// controls the protection of the credential itself; the index is always
    /// written unprotected — it must stay readable without a biometric
    /// prompt so startup discovery works before authentication.
    fn set_custom_tool_credential(
        &mut self,
        profile_name: Option<&str>,
        slug: &str,
        field: &str,
        value: String,
        biometric: bool,
    ) -> Result<(), String> {
        let key = format_custom_tool_key(profile_name, slug, field);

        // 1. Save the actual secret
        self.set_with_protection(&key, value, biometric)?;

        // 2. Update Index
        let current_index = self.get_index_from_keychain().unwrap_or_default();
        let new_index = add_to_index_csv(&current_index, &key);

        if new_index != current_index {
            self.set_with_protection(CUSTOM_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Updated custom tool index with new key: {}", key);
        }

        Ok(())
    }

    /// Delete a custom tool credential and update the index.
    fn delete_custom_tool_credential(
        &mut self,
        profile_name: Option<&str>,
        slug: &str,
        field: &str,
    ) -> Result<(), String> {
        let key = format_custom_tool_key(profile_name, slug, field);

        // 1. Delete the actual secret
        let _ = self.delete(&key);

        // 2. Update Index
        let current_index = self.get_index_from_keychain().unwrap_or_default();

        if !current_index.is_empty() {
            let new_index = remove_from_index_csv(&current_index, &key);
            // Same as set_custom_tool_credential: never biometric-protect the index.
            self.set_with_protection(CUSTOM_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Removed custom tool key from index: {}", key);
        }

        Ok(())
    }

    /// Get a Composio API key for a specific profile.
    fn get_composio_key(&self, profile_id: &str) -> Option<&String> {
        let key = composio_key_name(profile_id);
        self.get(&key)
    }

    /// Set a Composio API key for a specific profile.
    fn set_composio_key(&mut self, profile_id: &str, value: String) -> Result<(), String> {
        let key = composio_key_name(profile_id);
        self.set(&key, value)
    }

    /// Delete a Composio API key for a specific profile.
    fn delete_composio_key(&mut self, profile_id: &str) -> Result<(), String> {
        let key = composio_key_name(profile_id);
        self.delete(&key)
    }

    /// Get the API key for an LLM connector instance.
    fn get_llm_key(&self, connector_id: &str) -> Option<&String> {
        self.get(&llm_key_name(connector_id))
    }

    /// Set the API key for an LLM connector instance and keep the discovery
    /// index current so `load_all_from_keychain` finds it on next launch.
    /// `biometric` controls the protection of the key itself; the index is
    /// always written unprotected — it must stay readable without a
    /// biometric prompt so startup discovery works before authentication.
    fn set_llm_key(
        &mut self,
        connector_id: &str,
        value: String,
        biometric: bool,
    ) -> Result<(), String> {
        let key = llm_key_name(connector_id);
        self.set_with_protection(&key, value, biometric)?;

        let current_index = self
            .get_named_index_from_keychain(LLM_KEYS_INDEX_KEY)
            .unwrap_or_default();
        let new_index = add_to_index_csv(&current_index, &key);
        if new_index != current_index {
            self.set_with_protection(LLM_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Updated LLM connector key index with: {}", key);
        }
        Ok(())
    }

    /// Delete the API key for an LLM connector instance and update the index.
    fn delete_llm_key(&mut self, connector_id: &str) -> Result<(), String> {
        let key = llm_key_name(connector_id);
        let _ = self.delete(&key);

        let current_index = self
            .get_named_index_from_keychain(LLM_KEYS_INDEX_KEY)
            .unwrap_or_default();
        if !current_index.is_empty() {
            let new_index = remove_from_index_csv(&current_index, &key);
            // Same as set_llm_key: the index must never carry a biometric ACL.
            self.set_with_protection(LLM_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Removed LLM connector key from index: {}", key);
        }
        Ok(())
    }

    /// Get the feed URL for a calendar subscription.
    #[allow(dead_code)] // consumer is the Phase 2 ICS fetcher / Phase 4 settings UI
    fn get_cal_url(&self, subscription_id: &str) -> Option<&String> {
        self.get(&cal_url_key_name(subscription_id))
    }

    /// Set the feed URL for a calendar subscription and keep the discovery
    /// index current so `load_all_from_keychain` finds it on next launch.
    /// `biometric` controls the protection of the URL itself; the index is
    /// always written unprotected — it must stay readable without a
    /// biometric prompt so startup discovery works before authentication.
    #[allow(dead_code)] // consumer is the Phase 4 settings UI (URL paste)
    fn set_cal_url(
        &mut self,
        subscription_id: &str,
        value: String,
        biometric: bool,
    ) -> Result<(), String> {
        let key = cal_url_key_name(subscription_id);
        self.set_with_protection(&key, value, biometric)?;

        let current_index = self
            .get_named_index_from_keychain(CAL_KEYS_INDEX_KEY)
            .unwrap_or_default();
        let new_index = add_to_index_csv(&current_index, &key);
        if new_index != current_index {
            self.set_with_protection(CAL_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Updated calendar URL key index with: {}", key);
        }
        Ok(())
    }

    /// Delete the feed URL for a calendar subscription and update the index.
    #[allow(dead_code)] // consumer is the Phase 4 settings UI (remove subscription)
    fn delete_cal_url(&mut self, subscription_id: &str) -> Result<(), String> {
        let key = cal_url_key_name(subscription_id);
        let _ = self.delete(&key);

        let current_index = self
            .get_named_index_from_keychain(CAL_KEYS_INDEX_KEY)
            .unwrap_or_default();
        if !current_index.is_empty() {
            let new_index = remove_from_index_csv(&current_index, &key);
            // Same as set_cal_url: the index must never carry a biometric ACL.
            self.set_with_protection(CAL_KEYS_INDEX_KEY, new_index, false)?;
            tracing::info!("Removed calendar URL key from index: {}", key);
        }
        Ok(())
    }
}

// ============================================================================
// CREDENTIAL EXTRACTION HELPER
// ============================================================================

/// Extract custom tool credentials from a secrets map, optionally filtered by profile.
///
/// Returns a nested map: `Map<ToolkitSlug, Map<FieldName, Value>>`.
pub fn extract_custom_tool_credentials(
    profile_name: Option<&str>,
    secrets: &HashMap<String, String>,
) -> HashMap<String, HashMap<String, String>> {
    let mut result: HashMap<String, HashMap<String, String>> = HashMap::new();

    for (key, value) in secrets {
        if let Some((k_profile, slug, field)) = parse_custom_tool_key(key) {
            // Priority: Scoped Creds
            // Fallback: Global/Global Match if no profile specified
            let matches = match (profile_name, k_profile) {
                (Some(p), Some(kp)) if p == kp => true,
                (None, _) => true,       // Extract ALL if no filter
                (Some(_), None) => true, // Fallback: Allow global creds in a profile
                _ => false,
            };

            if matches {
                result.entry(slug).or_default().insert(field, value.clone());
            }
        }
    }
    result
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_custom_tool_key() {
        // Global (no profile)
        assert_eq!(
            format_custom_tool_key(None, "slack", "api_key"),
            "composio_tool_slack__api_key"
        );
        assert_eq!(
            format_custom_tool_key(None, "gmail", "client_secret"),
            "composio_tool_gmail__client_secret"
        );
        // Profile-scoped
        assert_eq!(
            format_custom_tool_key(Some("Puget"), "slack", "api_key"),
            "composio_tool_p:Puget:slack__api_key"
        );
    }

    #[test]
    fn test_parse_custom_tool_key_valid() {
        // Global key
        let result = parse_custom_tool_key("composio_tool_slack__api_key");
        assert_eq!(
            result,
            Some((None, "slack".to_string(), "api_key".to_string()))
        );
        // Profile-scoped key
        let scoped = parse_custom_tool_key("composio_tool_p:Puget:slack__api_key");
        assert_eq!(
            scoped,
            Some((
                Some("Puget".to_string()),
                "slack".to_string(),
                "api_key".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_custom_tool_key_invalid() {
        assert_eq!(parse_custom_tool_key("api_key"), None);
        assert_eq!(parse_custom_tool_key("composio_tool_slack"), None); // No separator
        assert_eq!(parse_custom_tool_key("composio_tool___field"), None); // Empty slug
        assert_eq!(parse_custom_tool_key("composio_tool_slug__"), None); // Empty field
    }

    #[test]
    fn test_key_belongs_to_toolkit() {
        assert!(key_belongs_to_toolkit(
            "composio_tool_slack__api_key",
            "slack"
        ));
        assert!(!key_belongs_to_toolkit(
            "composio_tool_gmail__api_key",
            "slack"
        ));
        assert!(!key_belongs_to_toolkit("api_key", "slack"));
    }

    #[test]
    fn test_parse_index_csv() {
        let keys = parse_index_csv("key1, key2 ,key3");
        assert_eq!(keys, vec!["key1", "key2", "key3"]);

        let empty_keys = parse_index_csv("");
        assert!(empty_keys.is_empty());
    }

    #[test]
    fn test_llm_key_name_and_index_roundtrip() {
        let key = llm_key_name("abc-123");
        assert_eq!(key, "llm_api_key_abc-123");
        assert!(key.starts_with(LLM_KEY_PREFIX));

        // Index round-trip: add two keys, remove one
        let idx = add_to_index_csv("", &llm_key_name("a"));
        let idx = add_to_index_csv(&idx, &llm_key_name("b"));
        assert_eq!(parse_index_csv(&idx).len(), 2);
        // Re-adding is a no-op
        let idx = add_to_index_csv(&idx, &llm_key_name("a"));
        assert_eq!(parse_index_csv(&idx).len(), 2);
        let idx = remove_from_index_csv(&idx, &llm_key_name("a"));
        assert_eq!(parse_index_csv(&idx), vec![llm_key_name("b")]);
    }

    #[test]
    fn test_cal_url_key_name_and_index_roundtrip() {
        let key = cal_url_key_name("sub-42");
        assert_eq!(key, "cal_url_sub-42");
        assert!(key.starts_with(CAL_URL_PREFIX));

        let idx = add_to_index_csv("", &cal_url_key_name("a"));
        let idx = add_to_index_csv(&idx, &cal_url_key_name("b"));
        assert_eq!(parse_index_csv(&idx).len(), 2);
        let idx = remove_from_index_csv(&idx, &cal_url_key_name("a"));
        assert_eq!(parse_index_csv(&idx), vec![cal_url_key_name("b")]);
    }

    #[test]
    fn test_add_to_index_csv() {
        assert_eq!(add_to_index_csv("", "key1"), "key1");
        assert_eq!(add_to_index_csv("key1", "key2"), "key1,key2");
        assert_eq!(add_to_index_csv("key1,key2", "key1"), "key1,key2"); // No duplicate
    }

    #[test]
    fn test_remove_from_index_csv() {
        assert_eq!(remove_from_index_csv("key1,key2,key3", "key2"), "key1,key3");
        assert_eq!(remove_from_index_csv("key1", "key1"), "");
        assert_eq!(remove_from_index_csv("key1,key2", "key3"), "key1,key2"); // Key not found
    }

    #[test]
    fn test_extract_custom_tool_credentials() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "composio_tool_slack__api_key".to_string(),
            "sk-123".to_string(),
        );
        secrets.insert(
            "composio_tool_slack__secret".to_string(),
            "sec-456".to_string(),
        );
        secrets.insert(
            "composio_tool_gmail__token".to_string(),
            "tok-789".to_string(),
        );
        secrets.insert("api_key".to_string(), "gemini-key".to_string()); // Not a tool cred

        let result = extract_custom_tool_credentials(None, &secrets);

        assert_eq!(result.len(), 2); // slack and gmail
        assert_eq!(
            result.get("slack").unwrap().get("api_key").unwrap(),
            "sk-123"
        );
        assert_eq!(
            result.get("slack").unwrap().get("secret").unwrap(),
            "sec-456"
        );
        assert_eq!(
            result.get("gmail").unwrap().get("token").unwrap(),
            "tok-789"
        );
    }
}
