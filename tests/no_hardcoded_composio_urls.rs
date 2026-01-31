/// Cargo test to prevent hardcoded Composio URLs.
/// This is a common regression when AIs or humans fix issues.
#[test]
fn no_hardcoded_composio_urls() {
    use std::fs;
    use std::path::Path;

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("mcp")
        .join("composio_client");

    let mut errors = Vec::new();

    for entry in fs::read_dir(&dir).expect("Failed to read composio_client dir") {
        let path = entry.unwrap().path();
        if path.extension().map(|e| e == "rs").unwrap_or(false) {
            // Skip constants.rs - it's the legitimate single source of truth
            if path.file_name().map(|n| n == "constants.rs").unwrap_or(false) {
                continue;
            }
            let content = fs::read_to_string(&path).unwrap();
            // Check each line for hardcoded URLs, skip comments
            for (i, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                // Skip comments and doc strings
                if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("/*") {
                    continue;
                }
                if line.contains("backend.composio.dev") {
                    errors.push(format!(
                        "{}:{}: Hardcoded Composio URL found: `{}`",
                        path.display(),
                        i + 1,
                        trimmed
                    ));
                }
            }
        }
    }

    assert!(
        errors.is_empty(),
        "Found hardcoded Composio URLs. Use `client.get_api_base_url()` instead.\n{}",
        errors.join("\n")
    );
}
