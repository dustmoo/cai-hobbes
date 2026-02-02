//! Biometric authentication module for macOS Touch ID integration.
//!
//! This module provides:
//! - Touch ID / password authentication via LAContext
//! - Authentication context for keychain session reuse
//! - Session duration management for single-prompt access
//!
//! The authenticated LAContext can be passed to keychain operations via
//! kSecUseAuthenticationContext to avoid repeated password prompts.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::NSString;
use objc2_local_authentication::{LAContext, LAPolicy};
use std::sync::mpsc;

/// Maximum duration for authentication reuse (5 minutes, the OS maximum)
const AUTH_REUSE_DURATION_SECONDS: f64 = 300.0;

/// Result of a biometric authentication attempt
#[derive(Debug)]
pub enum AuthResult {
    /// Authentication succeeded, context is ready for keychain operations
    Success(AuthContext),
    /// User cancelled the authentication prompt
    Cancelled,
    /// Authentication failed with an error message
    Failed(String),
    /// Biometric authentication is not available on this device
    NotAvailable(String),
}

/// Wrapper around LAContext that can be used for keychain operations.
///
/// After successful authentication, this context can be passed to keychain
/// queries via kSecUseAuthenticationContext to avoid repeated prompts.
///
/// The context remains valid for the duration of the app's lifetime or until
/// explicitly invalidated.
pub struct AuthContext {
    /// The authenticated LAContext - kept alive for keychain reuse
    pub(crate) inner: Retained<LAContext>,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthContext")
            .field("inner", &"LAContext")
            .finish()
    }
}

impl AuthContext {
    /// Check if biometric authentication is available on this device.
    ///
    /// Returns true if Touch ID or Face ID is available and configured.
    #[allow(dead_code)]
    pub fn is_biometrics_available() -> bool {
        // SAFETY: LAContext::new() is safe to call during normal operation
        let context = unsafe { LAContext::new() };

        // Check if device owner authentication (biometrics or passcode) can be evaluated
        // SAFETY: canEvaluatePolicy_error is safe with a valid policy
        unsafe {
            context
                .canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthentication)
                .is_ok()
        }
    }

    /// Prompt the user for biometric authentication (Touch ID or Face ID).
    ///
    /// This function will:
    /// 1. Create an LAContext with session reuse enabled
    /// 2. Display the system authentication prompt with the provided reason
    /// 3. Return an authenticated context that can be used for keychain operations
    ///
    /// The context is configured with a 5-minute reuse duration, meaning subsequent
    /// authentications within that window will succeed automatically if the device
    /// was unlocked with biometrics.
    ///
    /// # Arguments
    /// * `reason` - The localized reason shown to the user in the authentication prompt
    ///
    /// # Returns
    /// * `AuthResult::Success(context)` - Authentication succeeded
    /// * `AuthResult::Cancelled` - User cancelled the prompt
    /// * `AuthResult::Failed(error)` - Authentication failed
    /// * `AuthResult::NotAvailable(error)` - Biometrics not available
    pub fn authenticate(reason: &str) -> AuthResult {
        // SAFETY: LAContext::new() is safe to call
        let context = unsafe { LAContext::new() };

        // Set the reuse duration to allow session-based authentication
        // SAFETY: Setting this property is safe
        unsafe {
            context.setTouchIDAuthenticationAllowableReuseDuration(AUTH_REUSE_DURATION_SECONDS);
        }

        // Check if we can evaluate the policy first
        // SAFETY: canEvaluatePolicy_error is safe with valid policy
        let can_evaluate =
            unsafe { context.canEvaluatePolicy_error(LAPolicy::DeviceOwnerAuthentication) };

        if let Err(error) = can_evaluate {
            let error_msg = format!("{:?}", error);
            tracing::warn!("Biometric authentication not available: {}", error_msg);
            return AuthResult::NotAvailable(error_msg);
        }

        // Create the localized reason string
        let reason_ns = NSString::from_str(reason);

        // Use a channel to receive the async result synchronously
        // (evaluatePolicy uses a callback pattern)
        let (tx, rx) = mpsc::channel::<(bool, Option<String>)>();

        // Create the reply block that will be called when auth completes
        // SAFETY: The block captures tx by value and will be called exactly once
        let reply_block = RcBlock::new(
            move |success: objc2::runtime::Bool, error: *mut objc2_foundation::NSError| {
                let success = success.as_bool();
                let error_msg = if error.is_null() {
                    None
                } else {
                    // SAFETY: We checked for null, and NSError is valid if not null
                    let error_ref = unsafe { &*error };
                    Some(format!("{:?}", error_ref))
                };
                let _ = tx.send((success, error_msg));
            },
        );

        // Evaluate the policy - this will show the Touch ID prompt
        // SAFETY: evaluatePolicy_localizedReason_reply is safe with valid parameters
        // The block must be sendable (it is, via RcBlock)
        unsafe {
            context.evaluatePolicy_localizedReason_reply(
                LAPolicy::DeviceOwnerAuthentication,
                &reason_ns,
                &reply_block,
            );
        }

        // Wait for the authentication result with timeout
        // If Info.plist is missing NSFaceIDUsageDescription, the callback never fires
        // and we'd hang forever without this timeout.
        use std::time::Duration;
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok((true, _)) => {
                tracing::info!("Biometric authentication successful");
                AuthResult::Success(AuthContext { inner: context })
            }
            Ok((false, Some(error))) => {
                // Check if this is a user cancellation
                if error.contains("LAErrorUserCancel") || error.contains("-2") {
                    tracing::info!("User cancelled biometric authentication");
                    AuthResult::Cancelled
                } else {
                    tracing::warn!("Biometric authentication failed: {}", error);
                    AuthResult::Failed(error)
                }
            }
            Ok((false, None)) => {
                tracing::warn!("Biometric authentication failed without error");
                AuthResult::Failed("Authentication failed".to_string())
            }
            Err(e) => {
                // Timeout or channel disconnected - likely Info.plist misconfiguration
                tracing::error!(
                    "Biometric auth error: {} - ensure Info.plist has NSFaceIDUsageDescription and app is properly signed",
                    e
                );
                AuthResult::Failed(
                    "Authentication timed out. Check Info.plist and code signing.".to_string(),
                )
            }
        }
    }

    /// Get a reference to the underlying LAContext.
    ///
    /// This can be used to pass the context to keychain operations.
    #[allow(dead_code)]
    pub fn context(&self) -> &LAContext {
        &self.inner
    }

    /// Get the raw pointer to the underlying LAContext for FFI use.
    ///
    /// This returns the actual Objective-C object pointer, suitable for
    /// passing to Security framework as kSecUseAuthenticationContext.
    ///
    /// Note: This must use Retained::as_ptr() rather than casting a reference,
    /// as the Security framework expects the actual object pointer.
    pub fn as_ptr(&self) -> *const std::ffi::c_void {
        Retained::as_ptr(&self.inner) as *const std::ffi::c_void
    }

    /// Invalidate this authentication context.
    ///
    /// After calling this, the context can no longer be used for keychain operations
    /// and any in-progress operations will be cancelled.
    #[allow(dead_code)]
    pub fn invalidate(&self) {
        // SAFETY: invalidate() is safe to call on a valid context
        unsafe {
            self.inner.invalidate();
        }
        tracing::debug!("Authentication context invalidated");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_biometrics_availability_check() {
        // This test just verifies the function runs without crashing
        // Actual availability depends on the hardware
        let _available = AuthContext::is_biometrics_available();
    }
}
