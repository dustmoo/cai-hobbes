#![cfg(target_os = "macos")]

use security_framework::os::macos::keychain::SecKeychain;

const SERVICE_NAME: &str = "ai.clearmirror.cai-hobbes";

pub fn save_secret(account: &str, secret: &str) -> Result<(), String> {
    let keychain = SecKeychain::default().map_err(|e| e.to_string())?;
    keychain
        .set_generic_password(SERVICE_NAME, account, secret.as_bytes())
        .map_err(|e| format!("Failed to save secret to Keychain: {}", e))
}

pub fn retrieve_secret(account: &str) -> Result<String, String> {
    let keychain = SecKeychain::default().map_err(|e| e.to_string())?;
    match keychain.find_generic_password(SERVICE_NAME, account) {
        Ok((password, _item)) => {
            String::from_utf8(password.to_vec())
                .map_err(|e| format!("Failed to decode secret from Keychain: {}", e))
        }
        Err(e) if e.code() == -25300 => Err("Secret not found in Keychain.".to_string()), // errSecItemNotFound
        Err(e) => Err(format!("Failed to retrieve secret from Keychain: {}", e)),
    }
}

#[allow(dead_code)]
pub fn delete_secret(account: &str) -> Result<(), String> {
    let keychain = SecKeychain::default().map_err(|e| e.to_string())?;
    match keychain.find_generic_password(SERVICE_NAME, account) {
        Ok((_password, item)) => {
            item.delete();
            Ok(())
        }
        Err(e) if e.code() == -25300 => Ok(()), // errSecItemNotFound, already deleted
        Err(e) => Err(format!("Failed to delete secret from Keychain: {}", e)),
    }
}