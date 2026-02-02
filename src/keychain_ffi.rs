//! Low-level FFI wrappers for Security.framework keychain operations
//! with authentication context support.
//!
//! This module provides keychain access functions that can use a pre-authenticated
//! LAContext to avoid repeated password prompts. This is the key integration point
//! between LocalAuthentication and Security frameworks.

use crate::biometric_auth::AuthContext;
use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFMutableDictionary;
use core_foundation::string::CFString;
use std::ptr;

use crate::constants::SERVICE_NAME;
use crate::settings::is_sandboxed;

// Import the Security framework constants we need
// Note: extern blocks must be unsafe in edition 2024
unsafe extern "C" {
    static kSecClass: CFTypeRef;
    static kSecClassGenericPassword: CFTypeRef;
    static kSecAttrService: CFTypeRef;
    static kSecAttrAccount: CFTypeRef;
    static kSecMatchLimit: CFTypeRef;
    static kSecMatchLimitOne: CFTypeRef;
    static kSecReturnData: CFTypeRef;
    static kSecValueData: CFTypeRef;
    static kSecUseAuthenticationContext: CFTypeRef;
    static kSecAttrAccessControl: CFTypeRef;
    #[allow(dead_code)]
    static kSecAttrAccessible: CFTypeRef;
    static kSecAttrAccessibleWhenUnlocked: CFTypeRef;
    static kSecAttrAccessibleWhenUnlockedThisDeviceOnly: CFTypeRef;
    static kSecAttrAccessGroup: CFTypeRef;
    static kSecAttrSynchronizable: CFTypeRef;
    static kSecAttrSynchronizableAny: CFTypeRef;

    fn SecItemCopyMatching(query: CFTypeRef, result: *mut CFTypeRef) -> i32;
    fn SecItemAdd(attributes: CFTypeRef, result: *mut CFTypeRef) -> i32;
    fn SecItemDelete(query: CFTypeRef) -> i32;

    // SecAccessControl functions
    fn SecAccessControlCreateWithFlags(
        allocator: CFTypeRef, // CFAllocatorRef, use null for default
        protection: CFTypeRef,
        flags: u64, // SecAccessControlCreateFlags
        error: *mut CFTypeRef,
    ) -> CFTypeRef; // Returns SecAccessControlRef

    // CoreFoundation functions
    fn CFDictionarySetValue(theDict: CFTypeRef, key: CFTypeRef, value: CFTypeRef);
    fn CFRelease(cf: CFTypeRef);
}

// SecAccessControlCreateFlags - bitfield for access control
// See: https://developer.apple.com/documentation/security/secaccesscontrolcreateflags
const SEC_ACCESS_CONTROL_USER_PRESENCE: u64 = 1 << 0; // .userPresence - biometric OR passcode

const ERR_SEC_SUCCESS: i32 = 0;
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_USER_CANCELED: i32 = -128;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// The keychain access group - must match the keychain-access-groups in entitlements.
/// Format: `TeamID`.`BundleID`
/// Only used for sandboxed (App Store/TestFlight) builds - PRO builds use default access.
const KEYCHAIN_ACCESS_GROUP: &str = "ABXVW6PWCW.ai.clearmirror.cai-hobbes";

/// Helper to conditionally set access group (only for sandboxed builds)
unsafe fn set_access_group_if_sandboxed(query: &CFMutableDictionary) {
    if is_sandboxed() {
        let access_group = CFString::new(KEYCHAIN_ACCESS_GROUP);
        unsafe {
            dict_set(
                query,
                kSecAttrAccessGroup,
                access_group.as_concrete_TypeRef() as CFTypeRef,
            );
        }
    }
    // For PRO/Developer ID builds, omit access group to use default keychain access
}

/// Error types for keychain operations
#[derive(Debug, Clone)]
pub enum KeychainError {
    /// Item was not found in keychain
    NotFound,
    /// User cancelled authentication
    AuthCancelled,
    /// Authentication required but not provided
    AuthRequired,
    /// Failed to decode the password data
    DecodingError(String),
    /// Other Security framework error
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

// Helper to set values in dictionary without trait ambiguity issues
unsafe fn dict_set(dict: &CFMutableDictionary, key: CFTypeRef, value: CFTypeRef) {
    unsafe {
        CFDictionarySetValue(dict.as_concrete_TypeRef() as CFTypeRef, key, value);
    }
}

/// Query a generic password from the keychain using an authenticated context.
///
/// This function uses the provided `AuthContext` with `kSecUseAuthenticationContext`
/// to avoid prompting the user for authentication if the context is already
/// authenticated via biometrics.
///
/// # Arguments
/// * `account` - The account name (key) to look up
/// * `context` - An authenticated LAContext from biometric authentication
///
/// # Returns
/// * `Ok(String)` - The password value
/// * `Err(KeychainError)` - If the lookup failed
pub fn find_generic_password_with_context(
    account: &str,
    context: &AuthContext,
) -> Result<String, KeychainError> {
    let query = CFMutableDictionary::new();

    // Set up the query
    let service_name = CFString::new(SERVICE_NAME);
    let account_name = CFString::new(account);

    unsafe {
        dict_set(&query, kSecClass, kSecClassGenericPassword);
        dict_set(
            &query,
            kSecAttrService,
            service_name.as_concrete_TypeRef() as CFTypeRef,
        );
        dict_set(
            &query,
            kSecAttrAccount,
            account_name.as_concrete_TypeRef() as CFTypeRef,
        );
        set_access_group_if_sandboxed(&query);
        dict_set(&query, kSecMatchLimit, kSecMatchLimitOne);
        dict_set(
            &query,
            kSecReturnData,
            CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
        );

        // Search for both local and synchronizable items
        dict_set(&query, kSecAttrSynchronizable, kSecAttrSynchronizableAny);

        // Set the authentication context - this is the key to avoiding repeated prompts!
        // The LAContext from biometric auth will be used for this keychain access.
        // CRITICAL: Use as_ptr() to get the actual Objective-C object pointer,
        // NOT a cast of the Rust reference (which would be the wrong address).
        let context_ptr = context.as_ptr();
        dict_set(&query, kSecUseAuthenticationContext, context_ptr);
    }

    let mut result: CFTypeRef = ptr::null();

    let status =
        unsafe { SecItemCopyMatching(query.as_concrete_TypeRef() as CFTypeRef, &mut result) };

    match status {
        ERR_SEC_SUCCESS => {
            if result.is_null() {
                return Err(KeychainError::NotFound);
            }

            // Convert the CFTypeRef (which should be CFData) to bytes
            let data = unsafe { CFData::wrap_under_create_rule(result as *const _) };
            let bytes = data.bytes();

            String::from_utf8(bytes.to_vec())
                .map_err(|e| KeychainError::DecodingError(e.to_string()))
        }
        ERR_SEC_ITEM_NOT_FOUND => Err(KeychainError::NotFound),
        ERR_SEC_USER_CANCELED => Err(KeychainError::AuthCancelled),
        ERR_SEC_INTERACTION_NOT_ALLOWED => Err(KeychainError::AuthRequired),
        code => Err(KeychainError::SecurityError(code)),
    }
}

/// Query a generic password from the keychain without an authentication context.
///
/// This will prompt the user for authentication if the item requires it.
/// Use `find_generic_password_with_context` when you have an authenticated context
/// to avoid multiple prompts.
///
/// # Arguments
/// * `account` - The account name (key) to look up
///
/// # Returns
/// * `Ok(String)` - The password value
/// * `Err(KeychainError)` - If the lookup failed
pub fn find_generic_password(account: &str) -> Result<String, KeychainError> {
    let query = CFMutableDictionary::new();

    let service_name = CFString::new(SERVICE_NAME);
    let account_name = CFString::new(account);

    unsafe {
        dict_set(&query, kSecClass, kSecClassGenericPassword);
        dict_set(
            &query,
            kSecAttrService,
            service_name.as_concrete_TypeRef() as CFTypeRef,
        );
        dict_set(
            &query,
            kSecAttrAccount,
            account_name.as_concrete_TypeRef() as CFTypeRef,
        );
        set_access_group_if_sandboxed(&query);
        dict_set(&query, kSecMatchLimit, kSecMatchLimitOne);
        dict_set(
            &query,
            kSecReturnData,
            CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
        );

        // Search for both local and synchronizable items
        dict_set(&query, kSecAttrSynchronizable, kSecAttrSynchronizableAny);
    }

    let mut result: CFTypeRef = ptr::null();

    let status =
        unsafe { SecItemCopyMatching(query.as_concrete_TypeRef() as CFTypeRef, &mut result) };

    match status {
        ERR_SEC_SUCCESS => {
            if result.is_null() {
                return Err(KeychainError::NotFound);
            }

            let data = unsafe { CFData::wrap_under_create_rule(result as *const _) };
            let bytes = data.bytes();

            String::from_utf8(bytes.to_vec())
                .map_err(|e| KeychainError::DecodingError(e.to_string()))
        }
        ERR_SEC_ITEM_NOT_FOUND => Err(KeychainError::NotFound),
        ERR_SEC_USER_CANCELED => Err(KeychainError::AuthCancelled),
        ERR_SEC_INTERACTION_NOT_ALLOWED => Err(KeychainError::AuthRequired),
        code => Err(KeychainError::SecurityError(code)),
    }
}

/// Save a generic password to the keychain.
///
/// This will create a new item or update an existing one.
///
/// # Arguments
/// * `account` - The account name (key)
/// * `password` - The password value to store
///
/// # Returns
/// * `Ok(())` - Successfully saved
/// * `Err(KeychainError)` - If the save failed
pub fn set_generic_password(account: &str, password: &str) -> Result<(), KeychainError> {
    // First, try to delete any existing item
    let _ = delete_generic_password(account);

    let query = CFMutableDictionary::new();

    let service_name = CFString::new(SERVICE_NAME);
    let account_name = CFString::new(account);
    let password_data = CFData::from_buffer(password.as_bytes());

    unsafe {
        dict_set(&query, kSecClass, kSecClassGenericPassword);
        dict_set(
            &query,
            kSecAttrService,
            service_name.as_concrete_TypeRef() as CFTypeRef,
        );
        dict_set(
            &query,
            kSecAttrAccount,
            account_name.as_concrete_TypeRef() as CFTypeRef,
        );
        set_access_group_if_sandboxed(&query);
        dict_set(
            &query,
            kSecValueData,
            password_data.as_concrete_TypeRef() as CFTypeRef,
        );

        // Make the item synchronizable (syncs via iCloud) ONLY if sandboxed (provisioned)
        // PRO builds without provisioning profiles cannot use iCloud Keychain
        if is_sandboxed() {
            dict_set(
                &query,
                kSecAttrSynchronizable,
                CFBoolean::true_value().as_concrete_TypeRef() as CFTypeRef,
            );
        }

        // Use standard "WhenUnlocked" protection (required for sync, "ThisDeviceOnly" prevents it)
        dict_set(&query, kSecAttrAccessible, kSecAttrAccessibleWhenUnlocked);
    }

    let status = unsafe { SecItemAdd(query.as_concrete_TypeRef() as CFTypeRef, ptr::null_mut()) };

    match status {
        ERR_SEC_SUCCESS => Ok(()),
        code => Err(KeychainError::SecurityError(code)),
    }
}

/// Delete a generic password from the keychain.
///
/// # Arguments
/// * `account` - The account name (key) to delete
///
/// # Returns
/// * `Ok(())` - Successfully deleted (or already didn't exist)
/// * `Err(KeychainError)` - If the delete failed
pub fn delete_generic_password(account: &str) -> Result<(), KeychainError> {
    let query = CFMutableDictionary::new();

    let service_name = CFString::new(SERVICE_NAME);
    let account_name = CFString::new(account);

    unsafe {
        dict_set(&query, kSecClass, kSecClassGenericPassword);
        dict_set(
            &query,
            kSecAttrService,
            service_name.as_concrete_TypeRef() as CFTypeRef,
        );
        dict_set(
            &query,
            kSecAttrAccount,
            account_name.as_concrete_TypeRef() as CFTypeRef,
        );
        set_access_group_if_sandboxed(&query);

        // Ensure we find synchronizable items to delete them
        dict_set(&query, kSecAttrSynchronizable, kSecAttrSynchronizableAny);
    }

    let status = unsafe { SecItemDelete(query.as_concrete_TypeRef() as CFTypeRef) };

    match status {
        ERR_SEC_SUCCESS => Ok(()),
        ERR_SEC_ITEM_NOT_FOUND => Ok(()), // Already deleted
        code => Err(KeychainError::SecurityError(code)),
    }
}

/// Save a generic password with biometric protection (Touch ID / passcode).
///
/// Items saved with this function can be accessed without repeated password prompts
/// when using `find_generic_password_with_context` with an authenticated LAContext.
///
/// This creates the keychain item with `SecAccessControl` using the `.userPresence` flag,
/// which requires either biometric authentication (Touch ID/Face ID) or device passcode.
///
/// # Arguments
/// * `account` - The account name (key)
/// * `password` - The password value to store
///
/// # Returns
/// * `Ok(())` - Successfully saved with biometric protection
/// * `Err(KeychainError)` - If the save failed
pub fn set_generic_password_with_biometric_protection(
    account: &str,
    password: &str,
) -> Result<(), KeychainError> {
    // First, delete any existing item (might be unprotected)
    let _ = delete_generic_password(account);

    // Create the access control object with user presence requirement
    let access_control = unsafe {
        let mut error: CFTypeRef = ptr::null();
        let ac = SecAccessControlCreateWithFlags(
            ptr::null(),                                  // Use default allocator
            kSecAttrAccessibleWhenUnlockedThisDeviceOnly, // Device-only (biometrics can't sync)
            SEC_ACCESS_CONTROL_USER_PRESENCE,             // Requires biometric OR passcode
            &mut error,
        );

        if ac.is_null() {
            tracing::error!("Failed to create SecAccessControl: error = {:?}", error);
            if !error.is_null() {
                CFRelease(error);
            }
            return Err(KeychainError::SecurityError(-1));
        }

        ac
    };

    let query = CFMutableDictionary::new();
    let service_name = CFString::new(SERVICE_NAME);
    let account_name = CFString::new(account);
    let password_data = CFData::from_buffer(password.as_bytes());

    unsafe {
        dict_set(&query, kSecClass, kSecClassGenericPassword);
        dict_set(
            &query,
            kSecAttrService,
            service_name.as_concrete_TypeRef() as CFTypeRef,
        );
        dict_set(
            &query,
            kSecAttrAccount,
            account_name.as_concrete_TypeRef() as CFTypeRef,
        );
        set_access_group_if_sandboxed(&query);
        dict_set(
            &query,
            kSecValueData,
            password_data.as_concrete_TypeRef() as CFTypeRef,
        );

        // NOTE: Biometric items (kSecAttrAccessControl with userPresence) CANNOT be synchronizable.
        // The Secure Enclave binds them to this device. Do NOT set kSecAttrSynchronizable here.

        // This is the key: set the access control to require user presence!
        dict_set(&query, kSecAttrAccessControl, access_control);
    }

    let status = unsafe { SecItemAdd(query.as_concrete_TypeRef() as CFTypeRef, ptr::null_mut()) };

    // Clean up the access control object
    unsafe { CFRelease(access_control) };

    match status {
        ERR_SEC_SUCCESS => {
            tracing::debug!("Saved secret '{}' with biometric protection", account);
            Ok(())
        }
        code => {
            tracing::error!(
                "Failed to save secret '{}' with biometric protection: error = {}",
                account,
                code
            );
            Err(KeychainError::SecurityError(code))
        }
    }
}
