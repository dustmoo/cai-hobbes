//! Shared security utilities for file path validation.
//!
//! Centralized here to eliminate duplication between `InlineImage` (markdown renderer)
//! and `ImageClient` (image generation). Any future file-reading code path should
//! use `validate_safe_file_path()` to prevent arbitrary file exfiltration.

use std::path::PathBuf;

/// Validate that a file path is inside a known safe directory.
///
/// Accepts raw paths or `file://`-prefixed paths. Returns the canonical
/// `PathBuf` if the path is inside a safe directory, or `None` otherwise.
///
/// Safe directories:
/// - `dirs::config_dir()/com.hobbes.app` (settings, generated images)
/// - `dirs::data_dir()/com.hobbes.app`
/// - `std::env::temp_dir()` (transient downloads)
pub fn validate_safe_file_path(path: &str) -> Option<PathBuf> {
    // Strip file:// prefix and URL-decode %20 → space
    let raw = path
        .strip_prefix("file://")
        .unwrap_or(path)
        .replace("%20", " ");

    let canonical = std::path::Path::new(&raw)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&raw));

    let safe_dirs: Vec<PathBuf> = [
        dirs::config_dir().map(|d| d.join("com.hobbes.app")),
        dirs::data_dir().map(|d| d.join("com.hobbes.app")),
        Some(std::env::temp_dir()),
    ]
    .into_iter()
    .flatten()
    // Canonicalize safe_dirs too — on macOS, temp_dir() returns /tmp which is
    // a symlink to /private/tmp. Without this, canonicalize(file) → /private/tmp/...
    // would NOT match starts_with(/tmp/...).
    .map(|d| d.canonicalize().unwrap_or(d))
    .collect();

    if safe_dirs.iter().any(|dir| canonical.starts_with(dir)) {
        Some(canonical)
    } else {
        tracing::warn!(
            "Path validation rejected file outside safe directories: {}",
            canonical.display()
        );
        None
    }
}

/// Detect MIME type from file extension for image files.
pub fn mime_from_extension(path: &str) -> &'static str {
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_safe_path_inside_temp_dir() {
        // Create an actual file in temp dir so canonicalize succeeds
        let tmp = std::env::temp_dir();
        let test_file = tmp.join("hobbes_security_test.png");
        let mut f = std::fs::File::create(&test_file).expect("create test file");
        f.write_all(b"fake png").expect("write test file");

        let result = validate_safe_file_path(test_file.to_str().unwrap());
        assert!(result.is_some(), "Path inside temp_dir should be allowed");

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[test]
    fn test_safe_path_inside_config_dir() {
        // Create an actual file inside the app config directory
        let config = dirs::config_dir()
            .expect("config_dir should exist")
            .join("com.hobbes.app");
        std::fs::create_dir_all(&config).expect("create config dir");
        let test_file = config.join("hobbes_security_test.png");
        let mut f = std::fs::File::create(&test_file).expect("create test file");
        f.write_all(b"fake png").expect("write test file");

        let result = validate_safe_file_path(test_file.to_str().unwrap());
        assert!(
            result.is_some(),
            "Path inside config_dir/com.hobbes.app should be allowed"
        );

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[test]
    fn test_unsafe_path_etc_passwd() {
        let result = validate_safe_file_path("/etc/passwd");
        assert!(
            result.is_none(),
            "/etc/passwd should be rejected as outside safe directories"
        );
    }

    #[test]
    fn test_unsafe_path_ssh_key() {
        let home = dirs::home_dir().unwrap_or_default();
        let ssh_path = home.join(".ssh/id_rsa");
        let result = validate_safe_file_path(ssh_path.to_str().unwrap());
        assert!(
            result.is_none(),
            "~/.ssh/id_rsa should be rejected as outside safe directories"
        );
    }

    #[test]
    fn test_file_prefix_stripping() {
        // Create a real file in temp dir with file:// prefix
        let tmp = std::env::temp_dir();
        let test_file = tmp.join("hobbes_file_prefix_test.png");
        let mut f = std::fs::File::create(&test_file).expect("create test file");
        f.write_all(b"fake png").expect("write test file");

        let file_uri = format!("file://{}", test_file.to_str().unwrap());
        let result = validate_safe_file_path(&file_uri);
        assert!(
            result.is_some(),
            "file:// prefixed path inside temp_dir should be allowed"
        );

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }

    #[test]
    fn test_url_encoded_spaces() {
        // Create a file with spaces in the path
        let tmp = std::env::temp_dir();
        let test_file = tmp.join("hobbes space test.png");
        let mut f = std::fs::File::create(&test_file).expect("create test file");
        f.write_all(b"fake png").expect("write test file");

        let encoded_path = test_file
            .to_str()
            .unwrap()
            .replace(' ', "%20");
        let result = validate_safe_file_path(&encoded_path);
        assert!(
            result.is_some(),
            "URL-encoded path with spaces should be decoded and allowed"
        );

        // Cleanup
        let _ = std::fs::remove_file(&test_file);
    }
}
