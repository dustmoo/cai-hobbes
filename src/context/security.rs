/// An error type for secure storage operations.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum SecureStorageError {
    #[error("Encryption failed: {0}")]
    Encryption(String),
    #[error("Decryption failed: {0}")]
    Decryption(String),
    #[error("Platform-specific error: {0}")]
    Platform(String),
}

/// A trait for abstracting platform-native secure storage operations.
///
/// This allows for different implementations on macOS (Keychain) and
/// Windows (Cryptography API) while providing a consistent interface.
#[allow(dead_code)]
pub trait SecureStorage {
    /// Encrypts the given plaintext data.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, SecureStorageError>;

    /// Decrypts the given ciphertext data.
    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SecureStorageError>;
}