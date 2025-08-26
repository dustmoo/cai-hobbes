#![cfg(target_os = "macos")]

use macos_accessibility_client::accessibility;

/// Checks if the application has accessibility permissions and prompts the user if not.
/// Returns `true` if permissions are granted, `false` otherwise.
pub fn check_and_prompt_for_accessibility() -> bool {
    accessibility::application_is_trusted_with_prompt()
}