//! Keychain references in `mcp_servers.json`.
//!
//! A server's `env` values, `args` and `uri` may contain
//! `${keychain:NAME}`. The value lives in the keychain (stored from the MCP
//! config editor's Secrets box, under `mcp_secret_<NAME>`) and is filled in
//! when the server starts, so the file itself holds no keys. The resolved
//! config stays in memory only: `save_configs` always rebuilds from the file.

use super::McpServerConfig;

const OPEN: &str = "${keychain:";

/// Whether `name` can be used as a secret name: letters, digits, `_`, `-`
/// and `.`, at most 64 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The reference text for `name`, as typed into the config.
pub fn reference(name: &str) -> String {
    format!("{OPEN}{name}}}")
}

/// Replace every `${keychain:NAME}` in `text`. A name with no stored value
/// (or an unclosed reference) is added to `missing` and left as written.
fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>, missing: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find('}') else {
            missing.push(after.to_string());
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => {
                if !missing.iter().any(|m| m == name) {
                    missing.push(name.to_string());
                }
                out.push_str(&rest[start..start + OPEN.len() + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Fill in the keychain references in `config`'s env values, args and uri.
/// `Err` carries the message to show for the server when a referenced
/// secret isn't stored; the config is then not safe to start.
pub fn resolve(config: &mut McpServerConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    let mut missing = Vec::new();
    for value in config.env.values_mut() {
        if value.contains(OPEN) {
            *value = expand(value, lookup, &mut missing);
        }
    }
    if let Some(args) = config.args.as_mut() {
        for arg in args.iter_mut() {
            if arg.contains(OPEN) {
                *arg = expand(arg, lookup, &mut missing);
            }
        }
    }
    if let Some(uri) = config.uri.as_mut() {
        if uri.contains(OPEN) {
            *uri = expand(uri, lookup, &mut missing);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    missing.sort();
    Err(format!(
        "No stored secret named {}. Add it under MCP Servers → Installed → Secrets, or fix the name in the config.",
        missing
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "github" => Some("ghp_123".into()),
            "host" => Some("example.com".into()),
            _ => None,
        }
    }

    fn config() -> McpServerConfig {
        let mut c = McpServerConfig::native_stub("s".into(), String::new());
        c.command = Some("npx".into());
        c
    }

    #[test]
    fn references_are_filled_in_env_args_and_uri() {
        let mut c = config();
        c.env.insert("GITHUB_TOKEN".into(), "${keychain:github}".into());
        c.env.insert("PLAIN".into(), "as-is".into());
        c.args = Some(vec!["--header".into(), "Authorization: Bearer ${keychain:github}".into()]);
        c.uri = Some("https://${keychain:host}/mcp?key=${keychain:github}".into());
        resolve(&mut c, &lookup).unwrap();
        assert_eq!(c.env["GITHUB_TOKEN"], "ghp_123");
        assert_eq!(c.env["PLAIN"], "as-is");
        assert_eq!(c.args.unwrap()[1], "Authorization: Bearer ghp_123");
        assert_eq!(c.uri.unwrap(), "https://example.com/mcp?key=ghp_123");
    }

    #[test]
    fn a_missing_secret_stops_the_server_and_names_it_once() {
        let mut c = config();
        c.env.insert("A".into(), "${keychain:nope}".into());
        c.args = Some(vec!["${keychain:nope}".into(), "${keychain:github}".into()]);
        let err = resolve(&mut c, &lookup).unwrap_err();
        assert_eq!(err.matches("\"nope\"").count(), 1, "{err}");
        assert!(!err.contains("ghp_123"), "a resolved value never appears in the message");
        // The unresolved reference is left as written, never sent as a value.
        assert_eq!(c.env["A"], "${keychain:nope}");
    }

    #[test]
    fn an_unclosed_reference_is_an_error_not_a_panic() {
        let mut c = config();
        c.env.insert("A".into(), "x${keychain:github".into());
        assert!(resolve(&mut c, &lookup).is_err());
        assert_eq!(c.env["A"], "x${keychain:github");
    }

    #[test]
    fn a_stored_value_is_not_expanded_again() {
        let mut c = config();
        c.env.insert("A".into(), "${keychain:loop}".into());
        let l = |n: &str| (n == "loop").then(|| "${keychain:github}".to_string());
        resolve(&mut c, &l).unwrap();
        assert_eq!(c.env["A"], "${keychain:github}");
    }

    #[test]
    fn names_are_restricted() {
        assert!(valid_name("github_token-2.prod"));
        assert!(!valid_name(""));
        assert!(!valid_name("a b"));
        assert!(!valid_name("a}b"));
        assert!(!valid_name("a,b"), "commas would break the index");
        assert!(!valid_name(&"x".repeat(65)));
        assert_eq!(reference("github"), "${keychain:github}");
    }
}
