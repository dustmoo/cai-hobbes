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
//! 2. Grants the container SID access to the toolchain caches (npm, uv),
//!    the user temp dir, and each `--allow` path (read/write) — AppContainer
//!    processes are denied the entire user profile by default.
//! 3. Adds the internet/private-network client capabilities only with `--net`.
//! 4. Spawns the command inside the container via `cmd.exe /d /s /c` with
//!    inherited stdio (the MCP stdio transport passes straight through),
//!    inside a kill-on-close job object so the server dies with Hobbes.
//! 5. Waits and propagates the child's exit code.

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
        CreateWellKnownSid, WinCapabilityInternetClientSid,
        WinCapabilityPrivateNetworkClientServerSid, ACL, DACL_SECURITY_INFORMATION, PSID,
        SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
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
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
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

    /// Grant `access` on `path` to the container SID (inheritable ACE).
    /// Failures are warnings, not fatal — the server may not need the dir.
    fn grant_access(path: &str, sid: PSID, access: u32) {
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
                eprintln!("hobbes-sandbox: warning: cannot read ACL of {}", path);
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
                grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
                Trustee: trustee,
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let status = SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl);
            if status != ERROR_SUCCESS {
                eprintln!("hobbes-sandbox: warning: SetEntriesInAcl failed for {}", path);
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
            if status != ERROR_SUCCESS {
                eprintln!("hobbes-sandbox: warning: cannot grant access on {}", path);
            }
        }
    }

    /// Toolchain dirs the container needs so npx/uvx work at all.
    fn toolchain_grants() -> (Vec<String>, Vec<String>) {
        let mut rw = Vec::new();
        let mut ro = Vec::new();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            rw.push(format!("{}\\npm-cache", local));
            rw.push(format!("{}\\uv", local));
            rw.push(format!("{}\\Temp", local));
        }
        if let Ok(roaming) = std::env::var("APPDATA") {
            ro.push(format!("{}\\npm", roaming));
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            ro.push(format!("{}\\.local", profile));
        }
        (rw, ro)
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

        // Filesystem grants: rw for toolchain caches + user-allowed paths,
        // read/execute for npm's global shims and ~/.local (uv.exe).
        let (rw_dirs, ro_dirs) = toolchain_grants();
        for dir in &rw_dirs {
            let _ = std::fs::create_dir_all(dir);
            grant_access(dir, sid, FILE_ALL_ACCESS.0);
        }
        for dir in &ro_dirs {
            if std::path::Path::new(dir).exists() {
                grant_access(dir, sid, FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0);
            }
        }
        for dir in &opts.allow {
            grant_access(dir, sid, FILE_ALL_ACCESS.0);
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
            // "The current directory is invalid." Use the rw-granted temp dir.
            let workdir = std::env::var("LOCALAPPDATA")
                .map(|l| format!("{}\\Temp", l))
                .or_else(|_| std::env::var("SystemRoot"))
                .unwrap_or_else(|_| "C:\\Windows".to_string());
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
    }
}
