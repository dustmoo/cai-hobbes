#[cfg(test)]
mod tests {
    use crate::keychain_ffi;
    use crate::secret_manager::{SecretManager, COMPOSIO_KEY_PREFIX};

    // Mock constants
    const PROFILE_NAME: &str = "ReproProfile";
    const SECRET_VALUE: &str = "test-secret-value-123";

    #[test]
    fn test_composio_key_persistence_cycle() {
        // 1. Simulate SettingsPanel Save Logic
        let key_name = format!("{}{}", COMPOSIO_KEY_PREFIX, PROFILE_NAME);

        println!("Saving secret: {} -> {}", key_name, SECRET_VALUE);

        // Use keychain_ffi directly (with biometric protection if available)
        let save_result =
            keychain_ffi::set_generic_password_with_biometric_protection(&key_name, SECRET_VALUE)
                .or_else(|e| {
                    // Fallback to regular save without biometric protection
                    if let keychain_ffi::KeychainError::SecurityError(-34018) = e {
                        keychain_ffi::set_generic_password(&key_name, SECRET_VALUE)
                    } else {
                        Err(e)
                    }
                });
        assert!(
            save_result.is_ok(),
            "Failed to save secret: {:?}",
            save_result.err()
        );

        // 2. Simulate Main.rs Load Logic (without AuthContext / Biometrics initially to test fallback)
        let mut sm = SecretManager::new();

        // Load specifically this profile's key (simulating main.rs loop)
        // We use load_composio_key (synchronous/no-context) first as baseline
        sm.load_composio_key(PROFILE_NAME);

        let loaded = sm.get_composio_key(PROFILE_NAME);
        println!("Loaded secret (fallback): {:?}", loaded);

        // 3. Clean up
        let delete_result = keychain_ffi::delete_generic_password(&key_name);
        assert!(delete_result.is_ok(), "Failed to delete secret");
    }
}
