//! OS-level sandboxing for locally launched MCP server processes.
//!
//! All three platforms follow the same philosophy: broad read access minus
//! sensitive user data (running npx/uvx requires reading the toolchain and
//! system libraries, so a read allowlist is impractical), a strict write
//! allowlist (user-granted dirs + toolchain caches + temp), and a per-server
//! network toggle.
//!
//! - macOS: wraps the command with `sandbox-exec` and a generated Seatbelt
//!   profile.
//! - Linux: wraps the command with `bwrap` (bubblewrap) when it is installed.
//! - Windows: wraps the command with the `hobbes-sandbox.exe` helper shipped
//!   next to the main executable, which launches the server inside an
//!   AppContainer (see `apps/windows_app/src/sandbox_shim.rs`).
//!
//! When no OS sandbox is available (bwrap/shim missing), `sandbox_available()`
//! is false so registry installs don't default to sandboxed; an *explicit*
//! `sandbox: true` on such a system is an error rather than a silent
//! unsandboxed launch. The Tier-1 trust prompts (per-call approval for
//! registry installs) remain the safety layer either way.

use crate::mcp::manager::McpServerConfig;

// ============================================================================
// PLATFORM AVAILABILITY
// ============================================================================

/// Whether an OS sandbox can actually be applied on this machine.
/// Drives the default-on behavior for registry installs and the install
/// dialog's sandbox section.
pub fn sandbox_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::path::Path::new("/usr/bin/sandbox-exec").exists()
    }
    #[cfg(target_os = "linux")]
    {
        find_in_path("bwrap").is_some()
    }
    #[cfg(target_os = "windows")]
    {
        shim_path().is_some()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

/// Locate an executable on PATH (no shell involved).
#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)] // only called on Linux; compiled under test everywhere
fn find_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Common locations when launched from a desktop entry with a minimal PATH
    for dir in ["/usr/bin", "/usr/local/bin", "/bin"] {
        let candidate = std::path::Path::new(dir).join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Path to the Windows AppContainer launcher, shipped next to hobbes.exe.
#[cfg(target_os = "windows")]
fn shim_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let shim = exe.parent()?.join("hobbes-sandbox.exe");
    if shim.is_file() { Some(shim) } else { None }
}

// ============================================================================
// SHARED HELPERS
// ============================================================================

/// Escape a path for embedding in a Seatbelt profile string literal.
/// Control characters (newlines etc.) are stripped outright — they cannot be
/// represented safely and would otherwise allow profile-rule injection.
#[cfg(any(target_os = "macos", test))]
fn sb_escape(path: &str) -> String {
    path.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Directories under $HOME whose contents a sandboxed server must never read:
/// user documents, credentials, mail/message stores, browser profiles and the
/// app's own config (chat history, settings). Shared across platforms; each
/// backend adds platform-specific entries.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const SENSITIVE_HOME_DIRS: &[&str] = &[
    "Documents",
    "Desktop",
    "Pictures",
    "Downloads",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config",
    ".kube",
    ".docker",
];

/// Individual files under $HOME that commonly hold credentials.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const SENSITIVE_HOME_FILES: &[&str] = &[
    ".netrc",
    ".zsh_history",
    ".bash_history",
    ".node_repl_history",
    ".python_history",
];

// ============================================================================
// MACOS — SEATBELT
// ============================================================================

/// Generate the Seatbelt profile text for a server config.
#[cfg(any(target_os = "macos", test))]
pub fn seatbelt_profile(config: &McpServerConfig) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp".to_string());

    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         ; Process lifecycle\n\
         (allow process-exec*)\n\
         (allow process-fork)\n\
         (allow signal (target children))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow iokit-open)\n\
         ; Broad reads (toolchains, dyld, frameworks) minus sensitive data\n\
         (allow file-read*)\n\
         (allow file-ioctl)\n",
    );

    // Deny reads of sensitive user data even though reads are otherwise
    // broad. Allowed paths below re-enable these when granted (in Seatbelt,
    // a later matching rule overrides an earlier one).
    profile.push_str("; Sensitive-read denylist\n");
    let mut deny_dirs: Vec<String> = SENSITIVE_HOME_DIRS
        .iter()
        .map(|d| format!("{}/{}", home, d))
        .collect();
    deny_dirs.extend(
        [
            // Credential stores and personal data under ~/Library
            "Library/Keychains",
            "Library/Application Support",
            "Library/Messages",
            "Library/Mail",
            "Library/Safari",
            "Library/Cookies",
            "Library/HTTPStorages",
            "Library/Containers",
            "Library/Group Containers",
            "Library/CloudStorage",
            "Library/Accounts",
            ".zsh_sessions",
        ]
        .iter()
        .map(|d| format!("{}/{}", home, d)),
    );
    // The app's own data (sessions.json = full chat history, settings.json).
    // Usually inside ~/Library/Application Support and already covered, but
    // deny it explicitly in case the config dir lives elsewhere.
    if let Some(cfg_dir) = dirs::config_dir() {
        deny_dirs.push(
            cfg_dir
                .join("com.hobbes.app")
                .to_string_lossy()
                .to_string(),
        );
    }
    for dir in &deny_dirs {
        profile.push_str(&format!(
            "(deny file-read* (subpath \"{}\"))\n",
            sb_escape(dir)
        ));
    }
    for file in SENSITIVE_HOME_FILES {
        profile.push_str(&format!(
            "(deny file-read* (literal \"{}/{}\"))\n",
            sb_escape(&home),
            file
        ));
    }
    // uv reads its own config from ~/.config/uv; re-allow after the ~/.config
    // deny (later rule wins).
    profile.push_str(&format!(
        "(allow file-read* (subpath \"{}/.config/uv\"))\n",
        sb_escape(&home)
    ));

    // Write allowlist: user-granted dirs + toolchain caches + temp.
    profile.push_str("; Write allowlist\n");
    let mut writable: Vec<String> = vec![
        "/private/tmp".to_string(),
        "/private/var/folders".to_string(), // TMPDIR lives here on macOS
        format!("{}/.npm", home),
        format!("{}/.cache", home),
        format!("{}/.local", home),
        format!("{}/.uv", home),
    ];
    writable.extend(config.allowed_paths.iter().cloned());
    for path in &writable {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sb_escape(path)
        ));
        // Granted paths override the sensitive-read denials above.
        profile.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            sb_escape(path)
        ));
    }
    // /dev/null, ptys etc.
    profile.push_str("(allow file-write-data (literal \"/dev/null\"))\n");
    profile.push_str("(allow file-write* (regex #\"^/dev/tty\"))\n");

    if config.allow_network {
        profile.push_str("; Network enabled\n(allow network*)\n(allow system-socket)\n");
    } else {
        profile.push_str("; Network disabled (deny default covers it)\n");
        // Local IPC (unix sockets in temp) still allowed for tool internals
        profile.push_str("(allow network* (local unix))\n");
    }

    profile
}

/// Wrap `(command, args)` with the OS sandbox when the config asks for it.
/// Returns the possibly-rewritten pair. On failure to prepare the sandbox,
/// returns an error so callers surface it instead of silently running
/// unsandboxed.
#[cfg(target_os = "macos")]
pub fn wrap_command(
    config: &McpServerConfig,
    command: String,
    args: Vec<String>,
) -> Result<(String, Vec<String>), String> {
    if !config.sandbox_enabled() || command.is_empty() {
        return Ok((command, args));
    }

    let profile = seatbelt_profile(config);
    let dir = dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("com.hobbes.app")
        .join("sandbox_profiles");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create sandbox profile dir: {}", e))?;
    let profile_path = dir.join(format!("{}.sb", sanitize_name(&config.name)));
    std::fs::write(&profile_path, profile)
        .map_err(|e| format!("Failed to write sandbox profile: {}", e))?;

    let mut wrapped_args = vec![
        "-f".to_string(),
        profile_path.to_string_lossy().to_string(),
        command,
    ];
    wrapped_args.extend(args);
    tracing::info!(
        "Sandboxing MCP server '{}' via sandbox-exec (profile: {:?})",
        config.name,
        profile_path
    );
    Ok(("/usr/bin/sandbox-exec".to_string(), wrapped_args))
}

// ============================================================================
// LINUX — BUBBLEWRAP
// ============================================================================

/// Build the full bwrap argument list (excluding the `bwrap` binary itself,
/// including the wrapped command). `home` and `exists` are injected for
/// testability — mount sources must exist or bwrap refuses to start.
#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)] // only called on Linux; compiled under test everywhere
pub fn bwrap_args(
    config: &McpServerConfig,
    home: &str,
    exists: &dyn Fn(&str) -> bool,
    command: String,
    args: Vec<String>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        // Broad read-only root; later mounts override earlier ones.
        "--ro-bind",
        "/",
        "/",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        // Private temp — also hides other users'/apps' tmp files.
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    // Mask sensitive directories with empty tmpfs mounts.
    let mut masked: Vec<String> = SENSITIVE_HOME_DIRS
        .iter()
        .map(|d| format!("{}/{}", home, d))
        .collect();
    masked.push(format!("{}/.mozilla", home));
    masked.push(format!("{}/.pki", home));
    // gnome-keyring / KWallet stores live under ~/.local/share, which stays
    // writable for uv — mask just the credential dirs.
    masked.push(format!("{}/.local/share/keyrings", home));
    masked.push(format!("{}/.local/share/kwalletd", home));
    for dir in &masked {
        if exists(dir) {
            a.push("--tmpfs".to_string());
            a.push(dir.clone());
        }
    }
    // Mask credential files by binding /dev/null over them.
    for file in SENSITIVE_HOME_FILES {
        let path = format!("{}/{}", home, file);
        if exists(&path) {
            a.push("--ro-bind".to_string());
            a.push("/dev/null".to_string());
            a.push(path);
        }
    }
    // uv reads its own config from ~/.config/uv; re-expose it (read-only)
    // inside the ~/.config mask.
    let uv_cfg = format!("{}/.config/uv", home);
    if exists(&uv_cfg) {
        a.push("--ro-bind".to_string());
        a.push(uv_cfg.clone());
        a.push(uv_cfg);
    }

    // Write allowlist: toolchain caches + user-granted paths (rw binds).
    let mut writable: Vec<String> = [".npm", ".cache", ".local", ".uv"]
        .iter()
        .map(|d| format!("{}/{}", home, d))
        .collect();
    writable.extend(config.allowed_paths.iter().cloned());
    for path in &writable {
        if exists(path) {
            a.push("--bind".to_string());
            a.push(path.clone());
            a.push(path.clone());
        }
    }

    if !config.allow_network {
        a.push("--unshare-net".to_string());
    }

    a.push(command);
    a.extend(args);
    a
}

#[cfg(target_os = "linux")]
pub fn wrap_command(
    config: &McpServerConfig,
    command: String,
    args: Vec<String>,
) -> Result<(String, Vec<String>), String> {
    if !config.sandbox_enabled() || command.is_empty() {
        return Ok((command, args));
    }
    let bwrap = find_in_path("bwrap").ok_or_else(|| {
        "bubblewrap (bwrap) is not installed — install it or disable the sandbox for this server"
            .to_string()
    })?;
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    let exists = |p: &str| std::path::Path::new(p).exists();
    let wrapped_args = bwrap_args(config, &home, &exists, command, args);
    tracing::info!("Sandboxing MCP server '{}' via bwrap", config.name);
    Ok((bwrap.to_string_lossy().to_string(), wrapped_args))
}

// ============================================================================
// WINDOWS — APPCONTAINER (via hobbes-sandbox.exe)
// ============================================================================

#[cfg(target_os = "windows")]
pub fn wrap_command(
    config: &McpServerConfig,
    command: String,
    args: Vec<String>,
) -> Result<(String, Vec<String>), String> {
    if !config.sandbox_enabled() || command.is_empty() {
        return Ok((command, args));
    }
    let shim = shim_path().ok_or_else(|| {
        "hobbes-sandbox.exe not found next to the main executable — \
         reinstall Hobbes or disable the sandbox for this server"
            .to_string()
    })?;
    let mut wrapped_args = vec!["--name".to_string(), sanitize_name(&config.name)];
    if config.allow_network {
        wrapped_args.push("--net".to_string());
    }
    for path in &config.allowed_paths {
        wrapped_args.push("--allow".to_string());
        wrapped_args.push(path.clone());
    }
    wrapped_args.push("--".to_string());
    wrapped_args.push(command);
    wrapped_args.extend(args);
    tracing::info!(
        "Sandboxing MCP server '{}' via AppContainer shim ({:?})",
        config.name,
        shim
    );
    Ok((shim.to_string_lossy().to_string(), wrapped_args))
}

// ============================================================================
// OTHER PLATFORMS — PASSTHROUGH
// ============================================================================

/// No OS sandbox available on this platform — passthrough with a warning.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn wrap_command(
    config: &McpServerConfig,
    command: String,
    args: Vec<String>,
) -> Result<(String, Vec<String>), String> {
    if config.sandbox_enabled() {
        tracing::warn!(
            "Sandbox requested for MCP server '{}' but no OS sandbox is available on this platform",
            config.name
        );
    }
    Ok((command, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glama_config(allowed: Vec<String>, network: bool) -> McpServerConfig {
        McpServerConfig {
            name: "test-server".to_string(),
            command: Some("npx".to_string()),
            source: Some("glama".to_string()),
            sandbox: Some(true),
            allowed_paths: allowed,
            allow_network: network,
            ..Default::default()
        }
    }

    #[test]
    fn profile_includes_allowed_paths() {
        let config = glama_config(vec!["/Users/me/Projects".to_string()], true);
        let profile = seatbelt_profile(&config);
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-write* (subpath \"/Users/me/Projects\"))"));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn profile_omits_network_when_disabled() {
        let config = glama_config(vec![], false);
        let profile = seatbelt_profile(&config);
        assert!(!profile.contains("(allow network*)\n(allow system-socket)"));
        assert!(profile.contains("Network disabled"));
    }

    #[test]
    fn profile_escapes_quotes_in_paths() {
        let config = glama_config(vec![r#"/tmp/we"ird"#.to_string()], true);
        let profile = seatbelt_profile(&config);
        assert!(profile.contains(r#"/tmp/we\"ird"#));
    }

    #[test]
    fn sb_escape_strips_control_chars() {
        // A newline in a path must not become a profile-rule injection.
        let escaped = sb_escape("/tmp/x\"))\n(allow file-read* (subpath \"/");
        assert!(!escaped.contains('\n'));
        assert!(escaped.contains("\\\""));
    }

    #[test]
    fn profile_denies_sensitive_reads() {
        let config = glama_config(vec![], true);
        let profile = seatbelt_profile(&config);
        for needle in [
            ".ssh",
            "Keychains",
            "Library/Application Support",
            "Library/Messages",
            "Library/Mail",
            "Library/CloudStorage",
            "com.hobbes.app",
            ".netrc",
            ".zsh_history",
            ".config",
            ".docker",
            "Downloads",
        ] {
            assert!(
                profile.contains(needle),
                "profile missing denylist entry: {}",
                needle
            );
        }
        // uv config re-allowed after the ~/.config deny
        assert!(profile.contains(".config/uv"));
    }

    #[test]
    fn profile_denies_hobbes_config_dir() {
        let config = glama_config(vec![], true);
        let profile = seatbelt_profile(&config);
        let hobbes_dir = dirs::config_dir()
            .unwrap()
            .join("com.hobbes.app")
            .to_string_lossy()
            .to_string();
        assert!(profile.contains(&format!(
            "(deny file-read* (subpath \"{}\"))",
            sb_escape(&hobbes_dir)
        )));
    }

    #[test]
    fn config_serde_backcompat_and_new_fields() {
        // Pre-sandbox entry: defaults keep old behavior
        let old: McpServerConfig = serde_json::from_str(r#"{"command":"npx"}"#).unwrap();
        assert!(old.allow_network);
        assert!(old.secret_env.is_empty());
        assert!(!old.sandbox_enabled()); // manual install → no sandbox default

        let new: McpServerConfig = serde_json::from_str(
            r#"{"command":"npx","source":"glama","secret_env":["API_KEY"],
                "allowed_paths":["/x"],"allow_network":false,"sandbox":true}"#,
        )
        .unwrap();
        assert_eq!(new.secret_env, vec!["API_KEY"]);
        assert!(!new.allow_network);
        assert!(new.is_registry_install());
        assert!(new.sandbox_enabled());
    }

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize_name("owner/repo"), "owner_repo");
        assert_eq!(sanitize_name("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn bwrap_args_mask_sensitive_and_bind_allowed() {
        let config = glama_config(vec!["/home/me/Projects".to_string()], true);
        let exists = |_: &str| true;
        let args = bwrap_args(
            &config,
            "/home/me",
            &exists,
            "npx".to_string(),
            vec!["-y".to_string(), "pkg".to_string()],
        );
        let joined = args.join(" ");
        assert!(joined.contains("--ro-bind / /"));
        assert!(joined.contains("--tmpfs /home/me/.ssh"));
        assert!(joined.contains("--tmpfs /home/me/.config"));
        assert!(joined.contains("--tmpfs /home/me/.local/share/keyrings"));
        assert!(joined.contains("--ro-bind /dev/null /home/me/.netrc"));
        assert!(joined.contains("--bind /home/me/Projects /home/me/Projects"));
        assert!(joined.contains("--bind /home/me/.npm /home/me/.npm"));
        assert!(!joined.contains("--unshare-net"));
        // Command comes last
        assert_eq!(&args[args.len() - 3..], &["npx", "-y", "pkg"]);
    }

    #[test]
    fn bwrap_args_unshare_net_when_network_disabled() {
        let config = glama_config(vec![], false);
        let exists = |_: &str| false; // nothing exists → no masks/binds
        let args = bwrap_args(&config, "/home/me", &exists, "npx".to_string(), vec![]);
        let joined = args.join(" ");
        assert!(joined.contains("--unshare-net"));
        assert!(!joined.contains("--tmpfs /home/me/.ssh")); // skipped, doesn't exist
        assert!(joined.contains("--die-with-parent"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_is_passthrough_when_sandbox_off() {
        let mut config = glama_config(vec![], true);
        config.sandbox = Some(false);
        let (cmd, args) =
            wrap_command(&config, "npx".to_string(), vec!["-y".to_string()]).unwrap();
        assert_eq!(cmd, "npx");
        assert_eq!(args, vec!["-y"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn wrap_rewrites_to_sandbox_exec() {
        let config = glama_config(vec![], true);
        let (cmd, args) =
            wrap_command(&config, "npx".to_string(), vec!["-y".to_string(), "pkg".to_string()])
                .unwrap();
        assert_eq!(cmd, "/usr/bin/sandbox-exec");
        assert_eq!(args[0], "-f");
        assert!(args[1].ends_with("test-server.sb"));
        assert_eq!(&args[2..], &["npx", "-y", "pkg"]);
    }
}
