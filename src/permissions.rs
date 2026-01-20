#![cfg(target_os = "macos")]

use macos_accessibility_client::accessibility;

/// Represents the status of accessibility permissions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PermissionStatus {
    Granted,
    JustGranted,
    Denied,
}

/// Checks if the application has accessibility permissions and prompts the user if not.
pub fn check_and_prompt_for_accessibility() -> PermissionStatus {
    if accessibility::application_is_trusted() {
        PermissionStatus::Granted
    } else {
        if accessibility::application_is_trusted_with_prompt() {
            PermissionStatus::JustGranted
        } else {
            PermissionStatus::Denied
        }
    }
}
