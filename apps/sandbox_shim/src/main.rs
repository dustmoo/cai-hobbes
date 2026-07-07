//! hobbes-sandbox.exe — AppContainer launcher for locally installed MCP servers.
//!
//! Stable Rust cannot attach AppContainer security capabilities through
//! `std`/`tokio` `Command` (proc-thread attribute lists are nightly-only), so
//! Hobbes launches unvetted registry servers through this tiny broker instead:
//!
//! ```text
//! hobbes-sandbox.exe --name <container-name> [--net] [--allow <path>]... -- <command> [args...]
//! ```
//!
//! The broker:
//! 1. Creates (or reuses) an AppContainer profile named `Hobbes.<name>`.
//! 2. Routes all container-writable state (npm/uv caches, TEMP, cwd) to a
//!    per-container dir one level under the drive root — the only place a
//!    non-elevated broker can create that an app container can actually reach
//!    (the whole user profile, including the container's own package folder, is
//!    unreachable and un-grantable without elevation).
//! 3. Provisions a **container-readable copy** of the tool's toolchain (Node,
//!    etc.) and puts it first on the child's PATH, invoking the tool by bare
//!    name. An AppContainer can only reach an object whose ACL grants it both a
//!    normal SID (Users) *and* the app-container gate (ALL APPLICATION PACKAGES
//!    or the package SID); the system Node install has neither and is
//!    un-grantable without elevation, so it must be copied. Sets
//!    `NODE_OPTIONS=--preserve-symlinks*` so node doesn't crash stat-ing `C:\`.
//! 4. Adds the internet/private-network client capabilities only with `--net`.
//! 5. Spawns the command inside the container via `cmd.exe /d /s /c` with
//!    inherited stdio (the MCP stdio transport passes straight through),
//!    inside a kill-on-close job object so the server dies with Hobbes.
//! 6. Waits and propagates the child's exit code.
//!
//! See `WINDOWS_SANDBOX_FINDINGS.md` for the full AppContainer access model
//! this design is built on.

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("hobbes-sandbox is only functional on Windows");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    match win::run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("hobbes-sandbox: {}", e);
            std::process::exit(70);
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::{c_void, OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, SetHandleInformation, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
        HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
    };
    use windows::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, WinBuiltinAnyPackageSid, WinCapabilityInternetClientSid,
        WinCapabilityPrivateNetworkClientServerSid, ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION,
        PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        WELL_KNOWN_SID_TYPE,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    };
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::SystemServices::SE_GROUP_ENABLED;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED,
        EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    /// PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES — ProcThreadAttributeValue(9, FALSE, TRUE, FALSE)
    const ATTR_SECURITY_CAPABILITIES: usize = 0x0002_0009;

    struct Options {
        name: String,
        net: bool,
        allow: Vec<String>,
        command: Vec<String>,
    }

    fn parse_args() -> Result<Options, String> {
        let mut args = std::env::args().skip(1);
        let mut opts = Options {
            name: String::new(),
            net: false,
            allow: Vec::new(),
            command: Vec::new(),
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--name" => opts.name = args.next().ok_or("--name needs a value")?,
                "--net" => opts.net = true,
                "--allow" => opts.allow.push(args.next().ok_or("--allow needs a value")?),
                "--" => {
                    opts.command = args.collect();
                    break;
                }
                other => return Err(format!("unknown argument: {}", other)),
            }
        }
        if opts.name.is_empty() {
            return Err("--name is required".to_string());
        }
        if opts.command.is_empty() {
            return Err("no command given after --".to_string());
        }
        Ok(opts)
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Quote one argument per the MSVC command-line rules.
    fn quote_arg(arg: &str) -> String {
        // A bare arg is safe only if it has no whitespace/quote AND no trailing
        // backslash — args are joined and wrapped in cmd's `/c "..."`, so a
        // trailing `\` would otherwise escape the following quote.
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) && !arg.ends_with('\\') {
            return arg.to_string();
        }
        let mut out = String::from("\"");
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                    continue;
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                }
            }
            if c != '\\' {
                out.push(c);
            }
        }
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
        out
    }

    /// AppContainer profile name: `Hobbes.<name>`, ASCII-safe, ≤ 60 chars.
    fn container_name(raw: &str) -> String {
        let cleaned: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let mut name = format!("Hobbes.{}", cleaned);
        name.truncate(60);
        name
    }

    /// Create the AppContainer profile (or derive the SID if it exists).
    fn container_sid(name: &str) -> Result<PSID, String> {
        let name_w = wide(name);
        unsafe {
            match CreateAppContainerProfile(
                PCWSTR(name_w.as_ptr()),
                PCWSTR(name_w.as_ptr()),
                PCWSTR(name_w.as_ptr()),
                None,
            ) {
                Ok(sid) => Ok(sid),
                Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {
                    DeriveAppContainerSidFromAppContainerName(PCWSTR(name_w.as_ptr()))
                        .map_err(|e| format!("DeriveAppContainerSid failed: {}", e))
                }
                Err(e) => Err(format!("CreateAppContainerProfile failed: {}", e)),
            }
        }
    }

    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u8>, String> {
        // SECURITY_MAX_SID_SIZE = 68
        let mut buf = vec![0u8; 68];
        let mut len = buf.len() as u32;
        unsafe {
            CreateWellKnownSid(kind, None, PSID(buf.as_mut_ptr() as *mut c_void), &mut len)
                .map_err(|e| format!("CreateWellKnownSid failed: {}", e))?;
        }
        buf.truncate(len as usize);
        Ok(buf)
    }

    /// Add an ACE granting `access` on `path` to the container SID.
    /// `inherit` controls whether children inherit (full grants on leaf dirs)
    /// or not (traverse-only ACEs on ancestor dirs). `quiet` suppresses the
    /// failure warning for best-effort ancestor grants. Non-fatal throughout.
    fn grant_ace(path: &str, sid: PSID, access: u32, inherit: ACE_FLAGS, quiet: bool) {
        let path_w = wide(path);
        unsafe {
            let mut old_dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = windows::Win32::Security::PSECURITY_DESCRIPTOR::default();
            let status = GetNamedSecurityInfoW(
                PCWSTR(path_w.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut old_dacl),
                None,
                &mut sd,
            );
            if status != ERROR_SUCCESS {
                if !quiet {
                    eprintln!("hobbes-sandbox: warning: cannot read ACL of {}", path);
                }
                return;
            }
            let trustee = TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: PWSTR(sid.0 as *mut u16),
                ..Default::default()
            };
            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: access,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: inherit,
                Trustee: trustee,
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let status = SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl);
            if status != ERROR_SUCCESS {
                if !quiet {
                    eprintln!("hobbes-sandbox: warning: SetEntriesInAcl failed for {}", path);
                }
                return;
            }
            let status = SetNamedSecurityInfoW(
                PCWSTR(path_w.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new_dacl),
                None,
            );
            if status != ERROR_SUCCESS && !quiet {
                eprintln!("hobbes-sandbox: warning: cannot grant access on {}", path);
            }
        }
    }

    /// Full (inheritable) grant on a leaf directory the tool reads/writes.
    fn grant_access(path: &str, sid: PSID, access: u32) {
        grant_ace(path, sid, access, SUB_CONTAINERS_AND_OBJECTS_INHERIT, false);
    }

    /// An AppContainer can only reach a directory if it can *traverse* every
    /// ancestor. User-profile dirs (C:\Users\<u>\AppData\...) don't grant that
    /// to app-container SIDs, so a leaf grant alone is unreachable. Walk from
    /// the leaf's parent to the drive root adding a non-inherited execute
    /// (traverse) ACE — enough to pass through, not to list contents. System
    /// roots already allow traverse (grants there fail quietly); profile dirs
    /// are user-owned so the broker can amend them. `granted` dedups shared
    /// ancestors across multiple leaves.
    fn grant_traverse_chain(leaf: &str, sid: PSID, granted: &mut std::collections::HashSet<String>) {
        let mut cur = std::path::Path::new(leaf).parent();
        while let Some(dir) = cur {
            if dir.as_os_str().is_empty() {
                break;
            }
            let key = dir.to_string_lossy().to_string();
            // Stop once we hit a drive root like "C:\" (parent's parent is None).
            let at_root = dir.parent().is_none();
            if !at_root && granted.insert(key.clone()) {
                grant_ace(
                    &key,
                    sid,
                    FILE_GENERIC_EXECUTE.0,
                    ACE_FLAGS(0), // this directory only, no inheritance
                    true,
                );
            }
            cur = dir.parent();
        }
    }

    /// Base directory for all container-writable state, one level under the
    /// drive root (`C:\HobbesSandbox` by default, overridable via
    /// `HOBBES_SANDBOX_ROOT`).
    ///
    /// This is the crux of the Windows sandbox. An AppContainer can only reach
    /// a path if it can *traverse every ancestor*, and the ONLY directories
    /// that grant traversal to app-package SIDs by default are `C:\Windows` and
    /// `C:\Program Files` (via the ALL APPLICATION PACKAGES ACE). Everything a
    /// non-elevated user can write to — the whole user profile
    /// (`C:\Users\...`, including the container's own
    /// `%LOCALAPPDATA%\Packages\<moniker>\AC` folder), `C:\ProgramData`, etc. —
    /// lacks that ACE on its ancestor chain, and those ancestors can't be
    /// amended without elevation. So writable state under the profile is
    /// fundamentally unreachable.
    ///
    /// The drive root itself IS traversable by app containers (they launch
    /// executables out of `C:\Windows`, which requires passing through `C:\`).
    /// A directory placed directly under the root therefore has exactly one
    /// ancestor — the traversable root — plus dirs we create and own, on which
    /// we can grant the container SID full access. That makes the whole chain
    /// reachable without elevation.
    fn sandbox_state_root() -> String {
        std::env::var("HOBBES_SANDBOX_ROOT").unwrap_or_else(|_| {
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
            format!("{}\\HobbesSandbox", drive)
        })
    }

    /// Search `dirs` for `command` following cmd.exe's PATHEXT semantics, using
    /// `is_file` to probe (injected for testing). Pure — no env/FS access.
    ///
    /// Crucially cmd only ever runs a file whose extension is in PATHEXT, never
    /// an extensionless file. Node ships three siblings on Windows — `npx` (a
    /// Unix shell script, no extension), `npx.cmd`, and `npx.ps1` — and only
    /// `npx.cmd` is runnable by cmd. A naive "bare name first" search picks the
    /// Unix `npx`, which cmd can't execute ("Access is denied"). So we append a
    /// PATHEXT extension unless the caller already gave one.
    fn resolve_in(
        command: &str,
        dirs: &[std::path::PathBuf],
        exts: &[String],
        is_file: &dyn Fn(&std::path::Path) -> bool,
    ) -> Option<std::path::PathBuf> {
        let lower = command.to_ascii_lowercase();
        let has_exec_ext = exts.iter().any(|e| lower.ends_with(&e.to_ascii_lowercase()));
        for dir in dirs {
            if has_exec_ext {
                let candidate = dir.join(command);
                if is_file(&candidate) {
                    return Some(candidate);
                }
            } else {
                for ext in exts {
                    let candidate = dir.join(format!("{}{}", command, ext));
                    if is_file(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }

    /// Resolve a command to its full path the way cmd.exe would: an explicit
    /// path is used as-is, otherwise `resolve_in` searches PATH/PATHEXT. Used to
    /// locate the *source* toolchain to provision (see `provision_toolchain`).
    fn resolve_command_path(command: &str) -> Option<std::path::PathBuf> {
        if command.contains(['\\', '/', ':']) {
            let p = std::path::PathBuf::from(command);
            return p.is_file().then_some(p);
        }
        let exts: Vec<String> = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_string())
            .collect();
        let dirs: Vec<std::path::PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH")?).collect();
        resolve_in(command, &dirs, &exts, &|p| p.is_file())
    }

    /// Recursively copy `src` into `dst` (best-effort; skips entries it can't
    /// read/write rather than aborting the whole tree).
    fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if ft.is_dir() {
                let _ = copy_dir_recursive(&from, &to);
            } else {
                // Symlinks in a Node install are rare; copy the target contents.
                let _ = std::fs::copy(&from, &to);
            }
        }
        Ok(())
    }

    /// Provision a **container-readable copy** of the launched tool's toolchain
    /// directory, and return that directory (to prepend to the child's PATH).
    ///
    /// Why a copy is unavoidable: an AppContainer can only reach an object whose
    /// DACL grants it BOTH a normal token SID (Users) AND the app-container
    /// "gate" (ALL APPLICATION PACKAGES or the container's package SID). Node's
    /// own install dir (e.g. `C:\Program Files\nodejs`) has a replaced,
    /// non-inheriting ACL with no gate ACE, and is SYSTEM-owned so a
    /// non-elevated broker cannot add one — the container simply cannot execute
    /// it, and cmd's in-container PATH search silently skips it. We copy the
    /// toolchain to a broker-owned dir and grant ALL APPLICATION PACKAGES read
    /// (the gate); the inherited Users:RX supplies the right. The copy is shared
    /// across all containers and refreshed only when the source changes.
    fn provision_toolchain(command: &str, state_root: &str) -> Option<String> {
        let src_file = resolve_command_path(command)?;
        let src_dir = src_file.parent()?;
        let dir_name = src_dir.file_name()?.to_string_lossy().to_string();
        let dst = format!("{}\\toolchain\\{}", state_root, dir_name);
        let dst_path = std::path::PathBuf::from(&dst);

        // Staleness marker: the source dir's path + the launcher's mtime. Cheap
        // and good enough to catch a Node upgrade in place.
        let stamp = src_file
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let marker = dst_path.join(".hobbes-src");
        let want = format!("{}|{}", src_dir.to_string_lossy(), stamp);
        let fresh = std::fs::read_to_string(&marker)
            .map(|got| got == want)
            .unwrap_or(false);

        if !fresh {
            // Copy into place, then write the marker last so an interrupted
            // copy is not mistaken for a complete one.
            let _ = std::fs::remove_file(&marker);
            if let Err(e) = copy_dir_recursive(src_dir, &dst_path) {
                eprintln!("hobbes-sandbox: warning: toolchain copy failed: {}", e);
                return None;
            }
            let _ = std::fs::write(&marker, &want);
        }

        // Grant the app-container gate (ALL APPLICATION PACKAGES: read/execute)
        // on the copy. Users:RX is inherited from the state root, which together
        // makes the tree reachable+executable by every container. Idempotent.
        if let Ok(buf) = well_known_sid(WinBuiltinAnyPackageSid) {
            let pkg = PSID(buf.as_ptr() as *mut c_void);
            grant_access(&dst, pkg, FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0);
        }
        Some(dst)
    }

    fn make_inheritable(handle: HANDLE) {
        unsafe {
            let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(HANDLE_FLAG_INHERIT.0));
        }
    }

    pub fn run() -> Result<i32, String> {
        let opts = parse_args()?;

        let name = container_name(&opts.name);
        let sid = container_sid(&name)?;
        if std::env::var_os("HOBBES_SANDBOX_DEBUG").is_some() {
            unsafe {
                let mut s = PWSTR::null();
                if windows::Win32::Security::Authorization::ConvertSidToStringSidW(sid, &mut s)
                    .is_ok()
                {
                    eprintln!("hobbes-sandbox: container SID = {}", s.display());
                }
            }
        }

        // Writable state (npm/uv caches, TEMP, cwd) goes in a per-container
        // directory one level under the drive root (see sandbox_state_root) —
        // the only place a non-elevated broker can create that an app container
        // can actually reach. Redirect the toolchain there via env vars so
        // npx/uvx don't try to touch the unreachable user profile.
        let mut traversed = std::collections::HashSet::new();
        let base = format!("{}\\{}", sandbox_state_root(), name);
        let npm_cache = format!("{}\\npm-cache", base);
        let uv_cache = format!("{}\\uv-cache", base);
        let tmp = format!("{}\\tmp", base);
        for d in [&npm_cache, &uv_cache, &tmp] {
            let _ = std::fs::create_dir_all(d);
            grant_access(d, sid, FILE_ALL_ACCESS.0);
        }
        // The container must be able to traverse (and we may as well let it
        // read) every dir we just created between the drive root and the
        // leaves. grant_traverse_chain walks base's ancestors up to — but not
        // including — the drive root, which is already app-container
        // traversable. Grant base itself full access too.
        grant_access(&base, sid, FILE_ALL_ACCESS.0);
        grant_traverse_chain(&npm_cache, sid, &mut traversed);
        // Point the toolchain at the reachable caches. The broker is
        // single-shot, so mutating its own env (inherited by the child) is the
        // simplest way to inject these.
        std::env::set_var("npm_config_cache", &npm_cache);
        std::env::set_var("NPM_CONFIG_CACHE", &npm_cache);
        // Keep npm from reaching into the (unreachable) user profile for its
        // per-user and global config; point both at the sandbox and disable the
        // self-update notifier, which otherwise tries to write there.
        std::env::set_var("npm_config_userconfig", format!("{}\\npmrc", base));
        std::env::set_var("npm_config_globalconfig", format!("{}\\npmrc-global", base));
        std::env::set_var("npm_config_update_notifier", "false");
        std::env::set_var("NO_UPDATE_NOTIFIER", "1");
        // Pin the global prefix to a reachable dir, otherwise `npm prefix`
        // walks the cwd up to the drive root — which the container can't stat
        // ("Access is denied") — before falling back.
        std::env::set_var("npm_config_prefix", &base);
        std::env::set_var("UV_CACHE_DIR", &uv_cache);
        std::env::set_var("TMP", &tmp);
        std::env::set_var("TEMP", &tmp);
        // Point the profile-locating vars at the sandbox so tools that reach for
        // $HOME/%USERPROFILE% land somewhere reachable instead of tripping over
        // the denied real profile.
        std::env::set_var("USERPROFILE", &base);
        std::env::set_var("HOME", &base);
        std::env::set_var("HOMEDRIVE", "C:");
        std::env::set_var("APPDATA", &base);
        std::env::set_var("LOCALAPPDATA", &base);
        let workdir = tmp;

        // Provision a container-readable copy of the tool's toolchain (see
        // provision_toolchain) and put it FIRST on the child's PATH. The tool
        // is then invoked by its bare name so cmd's in-container PATH search
        // finds the reachable copy — an explicit path to it would trip cmd's
        // path canonicalisation, which needs a directory query the container
        // can't perform. The tool's real install dir (e.g. Program Files\nodejs)
        // stays later on PATH but is silently skipped as unreachable.
        if let Some(command) = opts.command.first() {
            if let Some(toolchain) = provision_toolchain(command, &sandbox_state_root()) {
                let path = std::env::var("PATH").unwrap_or_default();
                std::env::set_var("PATH", format!("{};{}", toolchain, path));
            } else {
                eprintln!(
                    "hobbes-sandbox: warning: could not provision a reachable copy of '{}'; \
                     the tool may fail to launch inside the container",
                    command
                );
            }
        }

        // Node walks a script's realpath up to the drive root during module
        // resolution; the container can traverse C:\ but not *stat* the root,
        // which crashes node with `EPERM: lstat 'C:\'`. --preserve-symlinks*
        // skips that realpath entirely. Merge with any inherited NODE_OPTIONS.
        {
            let flags = "--preserve-symlinks-main --preserve-symlinks";
            let merged = match std::env::var("NODE_OPTIONS") {
                Ok(existing) if !existing.trim().is_empty() => format!("{} {}", flags, existing),
                _ => flags.to_string(),
            };
            std::env::set_var("NODE_OPTIONS", merged);
        }
        // User-granted paths: grant the leaf and punch traverse ACEs up the
        // chain. NOTE: paths under C:\Users\<u> remain unreachable — C:\Users
        // itself isn't traversable by app containers and can't be amended
        // without elevation. Allowed paths should live outside the profile.
        for dir in &opts.allow {
            grant_access(dir, sid, FILE_ALL_ACCESS.0);
            grant_traverse_chain(dir, sid, &mut traversed);
        }

        // Capabilities: network only when requested.
        let mut cap_sids: Vec<Vec<u8>> = Vec::new();
        if opts.net {
            cap_sids.push(well_known_sid(WinCapabilityInternetClientSid)?);
            cap_sids.push(well_known_sid(WinCapabilityPrivateNetworkClientServerSid)?);
        }
        let mut capabilities: Vec<SID_AND_ATTRIBUTES> = cap_sids
            .iter()
            .map(|buf| SID_AND_ATTRIBUTES {
                Sid: PSID(buf.as_ptr() as *mut c_void),
                Attributes: SE_GROUP_ENABLED as u32,
            })
            .collect();
        let sec_caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: if capabilities.is_empty() {
                std::ptr::null_mut()
            } else {
                capabilities.as_mut_ptr()
            },
            CapabilityCount: capabilities.len() as u32,
            Reserved: 0,
        };

        // The MCP command is usually `npx`/`uvx` (a .cmd shim) — run through
        // cmd.exe so PATHEXT resolution works. cmd.exe lives in System32,
        // which AppContainers may read/execute by default.
        let inner: Vec<String> = opts.command.iter().map(|a| quote_arg(a)).collect();
        let system32 = std::env::var("SystemRoot")
            .map(|r| format!("{}\\System32", r))
            .unwrap_or_else(|_| "C:\\Windows\\System32".to_string());
        let cmd_exe = format!("{}\\cmd.exe", system32);
        let cmdline = format!("{} /d /s /c \"{}\"", quote_arg(&cmd_exe), inner.join(" "));
        if std::env::var_os("HOBBES_SANDBOX_DEBUG").is_some() {
            eprintln!("hobbes-sandbox: workdir = {}", workdir);
            eprintln!("hobbes-sandbox: cmdline = {}", cmdline);
        }
        let mut cmdline_w: Vec<u16> = OsString::from(&cmdline)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            // Attribute list carrying the AppContainer capabilities.
            let mut attr_size: usize = 0;
            let _ = InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut attr_size,
            );
            let mut attr_buf = vec![0u8; attr_size];
            let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut c_void);
            InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size)
                .map_err(|e| format!("InitializeProcThreadAttributeList failed: {}", e))?;
            UpdateProcThreadAttribute(
                attr_list,
                0,
                ATTR_SECURITY_CAPABILITIES,
                Some(&sec_caps as *const _ as *const c_void),
                std::mem::size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
            .map_err(|e| format!("UpdateProcThreadAttribute failed: {}", e))?;

            // Inherit our stdio straight through to the child.
            let stdin = GetStdHandle(STD_INPUT_HANDLE).unwrap_or_default();
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
            let stderr = GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default();
            for h in [stdin, stdout, stderr] {
                make_inheritable(h);
            }

            let mut siex = STARTUPINFOEXW::default();
            siex.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            siex.StartupInfo.hStdInput = stdin;
            siex.StartupInfo.hStdOutput = stdout;
            siex.StartupInfo.hStdError = stderr;
            siex.lpAttributeList = attr_list;

            // The child must start in a directory the container can access —
            // an inherited cwd under the user profile (Hobbes sets $HOME) is
            // denied to the container SID and makes cmd.exe fail with
            // "The current directory is invalid." Use the container-reachable
            // temp dir resolved above.
            let workdir_w = wide(&workdir);

            let mut pi = PROCESS_INFORMATION::default();
            let create_result = CreateProcessW(
                PCWSTR::null(),
                PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                true,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                None,
                PCWSTR(workdir_w.as_ptr()),
                &siex.StartupInfo,
                &mut pi,
            );
            DeleteProcThreadAttributeList(attr_list);
            if std::env::var_os("HOBBES_SANDBOX_DEBUG").is_some() {
                eprintln!("hobbes-sandbox: CreateProcessW result = {:?}", create_result);
            }
            create_result.map_err(|e| format!("CreateProcessW failed: {}", e))?;

            // Kill-on-close job: if Hobbes (or this broker) dies, so does the
            // server and everything it spawned. On any setup failure the
            // still-suspended child is terminated instead of leaking.
            let job_setup = (|| -> Result<HANDLE, String> {
                let job = CreateJobObjectW(None, PCWSTR::null())
                    .map_err(|e| format!("CreateJobObjectW failed: {}", e))?;
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .map_err(|e| format!("SetInformationJobObject failed: {}", e))?;
                AssignProcessToJobObject(job, pi.hProcess)
                    .map_err(|e| format!("AssignProcessToJobObject failed: {}", e))?;
                Ok(job)
            })();
            let job = match job_setup {
                Ok(job) => job,
                Err(e) => {
                    let _ = TerminateProcess(pi.hProcess, 1);
                    let _ = CloseHandle(pi.hThread);
                    let _ = CloseHandle(pi.hProcess);
                    return Err(e);
                }
            };
            ResumeThread(pi.hThread);

            WaitForSingleObject(pi.hProcess, INFINITE);
            let mut code: u32 = 1;
            let _ = GetExitCodeProcess(pi.hProcess, &mut code);
            if std::env::var_os("HOBBES_SANDBOX_DEBUG").is_some() {
                eprintln!("hobbes-sandbox: child exit code = {} (0x{:x})", code, code);
            }
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(job);

            Ok(code as i32)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quoting_matches_msvc_rules() {
            assert_eq!(quote_arg("simple"), "simple");
            assert_eq!(quote_arg("has space"), "\"has space\"");
            assert_eq!(quote_arg("tr\"icky"), "\"tr\\\"icky\"");
            assert_eq!(quote_arg("ends\\"), "\"ends\\\\\"");
        }

        #[test]
        fn container_name_is_sanitized_and_bounded() {
            assert_eq!(container_name("my-server"), "Hobbes.my-server");
            assert_eq!(container_name("we ird/name"), "Hobbes.we_ird_name");
            assert!(container_name(&"x".repeat(100)).len() <= 60);
        }

        #[test]
        fn resolve_skips_extensionless_and_prefers_pathext() {
            use std::path::{Path, PathBuf};
            let dir = PathBuf::from("C:\\tc\\nodejs");
            let dirs = [dir.clone()];
            let exts: Vec<String> = [".COM", ".EXE", ".CMD"].iter().map(|s| s.to_string()).collect();
            // Node ships all three; only npx.CMD is runnable by cmd.
            let present = |p: &Path| {
                matches!(
                    p.to_string_lossy().as_ref(),
                    "C:\\tc\\nodejs\\npx" | "C:\\tc\\nodejs\\npx.CMD" | "C:\\tc\\nodejs\\npx.ps1"
                )
            };
            let got = resolve_in("npx", &dirs, &exts, &present).unwrap();
            assert_eq!(got, dir.join("npx.CMD"), "must skip the extensionless Unix script");

            // A name that already carries an exec extension is used verbatim.
            let only_cmd = |p: &Path| p.to_string_lossy().ends_with("node.exe");
            let exts2: Vec<String> = [".EXE"].iter().map(|s| s.to_string()).collect();
            assert_eq!(
                resolve_in("node.exe", &dirs, &exts2, &only_cmd).unwrap(),
                dir.join("node.exe")
            );
        }
    }
}
