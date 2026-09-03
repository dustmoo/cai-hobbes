//! The native terminal: one persistent PTY shell per chat session, exposed
//! as the `hobbes-terminal` virtual MCP server.
//!
//! Deliberately NOT a builtin (`dispatch_builtin_tool`): terminal commands
//! are the highest-risk tool in the app, so they fall through to the normal
//! MCP permission flow — the approval bubble on first use, the per-server
//! settings toggle, and the marketplace loaded/on-demand/disabled dropdown.
//! Registration mirrors `hobbes-native-image`, the one native client with a
//! real executor arm.
//!
//! Concurrency shape: all PTY reads happen on a dedicated OS thread per
//! shell, feeding an unbounded byte channel; the async side only ever awaits
//! `tokio::time::timeout(_, rx.recv())`, so a hung command can never wedge
//! the turn (there is no global tool timeout upstream). The reader thread
//! holds only the channel sender — never the session Arc — so Drop runs.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use portable_pty::{CommandBuilder, PtySize};
use rmcp::model::Tool;

pub const HOBBES_TERMINAL_SERVER: &str = "hobbes-terminal";

/// Output kept per command (the tail survives; a marker notes truncation).
const OUTPUT_CAP_BYTES: usize = 100 * 1024;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 30;
const MAX_EXEC_TIMEOUT_SECS: u64 = 300;
const DEFAULT_TAIL_TIMEOUT_SECS: u64 = 5;
const MAX_TAIL_TIMEOUT_SECS: u64 = 60;

/// Per-call session context resolved by the CALLER — the manager has no
/// SessionState/PlannerState, so the call site passes the chat session id,
/// resolved working directory, and project. Built for EVERY tool call now
/// (not just terminal ones): the trust-rule gate and decision log use it;
/// the terminal remains its only executor-side consumer.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalCtx {
    pub session_id: String,
    pub cwd: String,
    pub project_id: Option<String>,
}

struct RunningCmd {
    sentinel: String,
    command: String,
}

struct PtySession {
    writer: std::sync::Mutex<Box<dyn Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Kept alive for the pty's lifetime; dropping it hangs up the shell.
    _master: std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    output_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    running: std::sync::Mutex<Option<RunningCmd>>,
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

#[derive(Clone, Default)]
pub struct TerminalClient {
    sessions: Arc<tokio::sync::Mutex<HashMap<String, Arc<PtySession>>>>,
}

// ── Pure helpers (unit-tested) ──────────────────────────────────────────────

/// Find `__HOBBES_DONE_<nonce>__ <code>` in the accumulated bytes; return
/// (output before it, exit code). The tty ECHOES the typed command, so the
/// first occurrence is usually the echoed printf format (followed by `%d`,
/// not a number) — scan every occurrence until one parses.
pub(crate) fn split_at_sentinel(buf: &[u8], sentinel: &str) -> Option<(String, i32)> {
    let text = String::from_utf8_lossy(buf);
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(sentinel) {
        let idx = search + rel;
        let after = &text[idx + sentinel.len()..];
        if let Some(code) = after
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<i32>().ok())
        {
            return Some((text[..idx].to_string(), code));
        }
        search = idx + sentinel.len();
    }
    None
}

/// Strip ANSI escapes and keep the LAST `cap` bytes (UTF-8-safe), marking
/// truncation.
pub(crate) fn clean_output(raw: &str, cap: usize) -> String {
    let stripped = strip_ansi_escapes::strip_str(raw);
    let trimmed = stripped.trim_matches(|c| c == '\r' || c == '\n');
    if trimmed.len() <= cap {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - cap;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[output truncated — showing last {}KB]\n{}",
        cap / 1024,
        &trimmed[start..]
    )
}

/// Single-quote a path for the `( cd '…' && … )` cwd-override subshell.
pub(crate) fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// session.project_id → planner Project.path → Settings.project_folder →
/// $HOME. Each step must exist on disk to win.
pub fn resolve_session_cwd(
    session: &crate::session::Session,
    planner: &crate::todo::PlannerState,
    settings: &crate::settings::Settings,
) -> String {
    let dir_ok = |p: &str| std::path::Path::new(p).is_dir().then(|| p.to_string());
    session
        .project_id
        .as_deref()
        .and_then(|pid| planner.projects.iter().find(|p| p.id == pid))
        .and_then(|p| p.path.as_deref())
        .and_then(crate::services::project_tagger::norm_path)
        .and_then(|p| dir_ok(&p))
        .or_else(|| settings.project_folder.as_deref().and_then(dir_ok))
        .or_else(|| dirs::home_dir().map(|h| h.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "/".to_string())
}

// ── Shell lifecycle ─────────────────────────────────────────────────────────

fn spawn_shell(cwd: &str) -> Result<PtySession, String> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty failed: {e}"))?;

    // -f (NO_RCS): a deterministic shell — the user's .zshrc (prompt themes,
    // instant-prompt plugins) can wedge a dumb pty. Login PATH is recovered
    // explicitly in the init line by sourcing the zprofiles.
    let mut cmd = CommandBuilder::new("/bin/zsh");
    cmd.arg("-f");
    cmd.cwd(cwd);
    cmd.env("TERM", "dumb");
    cmd.env("NO_COLOR", "1");
    cmd.env("CLICOLOR", "0");
    cmd.env("PAGER", "cat");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("HOBBES_TERMINAL", "1");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("shell spawn failed: {e}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("pty reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("pty writer: {e}"))?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let session = PtySession {
        writer: std::sync::Mutex::new(writer),
        child: std::sync::Mutex::new(child),
        _master: std::sync::Mutex::new(pair.master),
        output_rx: tokio::sync::Mutex::new(rx),
        running: std::sync::Mutex::new(None),
    };

    // Neuter prompts/echo so the sentinel protocol sees clean output, then
    // recover the login PATH the -f flag skipped.
    session.write_line(
        "unsetopt ZLE PROMPT_SP PROMPT_CR 2>/dev/null; PS1=''; PROMPT=''; RPROMPT=''; \
         stty -echo 2>/dev/null; export TERM=dumb; \
         [ -f /etc/zprofile ] && source /etc/zprofile >/dev/null 2>&1; \
         [ -f \"$HOME/.zprofile\" ] && source \"$HOME/.zprofile\" >/dev/null 2>&1; true",
    )?;
    Ok(session)
}

impl PtySession {
    fn write_line(&self, line: &str) -> Result<(), String> {
        let mut w = self.writer.lock().map_err(|_| "pty writer poisoned")?;
        w.write_all(line.as_bytes())
            .and_then(|_| w.write_all(b"\n"))
            .and_then(|_| w.flush())
            .map_err(|e| format!("pty write failed: {e}"))
    }
}

// ── Client ──────────────────────────────────────────────────────────────────

impl TerminalClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn kill_session(&self, session_id: &str) {
        // Drop runs the kill.
        self.sessions.lock().await.remove(session_id);
    }

    pub async fn shutdown_all(&self) {
        self.sessions.lock().await.clear();
    }

    async fn session_for(&self, ctx: &TerminalCtx) -> Result<Arc<PtySession>, String> {
        let mut map = self.sessions.lock().await;
        if let Some(s) = map.get(&ctx.session_id) {
            // A dead shell (crashed/killed externally) respawns transparently.
            let alive = s
                .child
                .lock()
                .ok()
                .map(|mut c| c.try_wait().ok().flatten().is_none())
                .unwrap_or(false);
            if alive {
                return Ok(s.clone());
            }
            map.remove(&ctx.session_id);
        }
        let fresh = Arc::new(spawn_shell(&ctx.cwd)?);
        // Drain the init sentinel-less output briefly on first exec instead —
        // the sentinel protocol makes any residue harmless.
        map.insert(ctx.session_id.clone(), fresh.clone());
        Ok(fresh)
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: Option<TerminalCtx>,
    ) -> Result<rmcp::model::CallToolResult, String> {
        let Some(ctx) = ctx else {
            return Err("Terminal tools require a chat session context.".to_string());
        };
        let text = match name {
            "HOBBES_TERMINAL_EXEC" => self.exec(&ctx, &args).await?,
            "HOBBES_TERMINAL_TAIL" => self.tail(&ctx, &args).await?,
            "HOBBES_TERMINAL_RESET" => {
                self.kill_session(&ctx.session_id).await;
                "Shell killed. The next HOBBES_TERMINAL_EXEC starts a fresh one in the \
                 session's working directory."
                    .to_string()
            }
            other => return Err(format!("unknown terminal tool '{other}'")),
        };
        Ok(rmcp::model::CallToolResult {
            content: vec![rmcp::model::Content::text(text)],
            is_error: Some(false),
            structured_content: None,
            meta: None,
        })
    }

    async fn exec(&self, ctx: &TerminalCtx, args: &serde_json::Value) -> Result<String, String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or("missing required 'command'")?;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
            .clamp(1, MAX_EXEC_TIMEOUT_SECS);

        let session = self.session_for(ctx).await?;
        {
            let running = session.running.lock().map_err(|_| "state poisoned")?;
            if let Some(r) = &*running {
                return Err(format!(
                    "A command is still running in this shell: `{}`. Use \
                     HOBBES_TERMINAL_TAIL to read its output, or \
                     HOBBES_TERMINAL_RESET to kill the shell.",
                    r.command
                ));
            }
        }

        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let sentinel = format!("__HOBBES_DONE_{nonce}__");
        let wrapped = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(dir) if !dir.trim().is_empty() => {
                format!("( cd {} && {} )", shell_quote(dir.trim()), command)
            }
            _ => command.to_string(),
        };
        session.write_line(&format!("{wrapped}\nprintf '\\n{sentinel} %d\\n' $?"))?;

        match self
            .read_until_sentinel(&session, &sentinel, timeout_secs)
            .await?
        {
            Some((output, code)) => Ok(format!("{}\n(exit code: {code})", clean_output(&output, OUTPUT_CAP_BYTES))),
            None => {
                *session.running.lock().map_err(|_| "state poisoned")? = Some(RunningCmd {
                    sentinel,
                    command: command.to_string(),
                });
                Ok(format!(
                    "Command still running after {timeout_secs}s. Use HOBBES_TERMINAL_TAIL \
                     to keep reading output, or HOBBES_TERMINAL_RESET to kill the shell."
                ))
            }
        }
    }

    async fn tail(&self, ctx: &TerminalCtx, args: &serde_json::Value) -> Result<String, String> {
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TAIL_TIMEOUT_SECS)
            .clamp(1, MAX_TAIL_TIMEOUT_SECS);
        let session = {
            let map = self.sessions.lock().await;
            map.get(&ctx.session_id).cloned()
        }
        .ok_or("No shell is running for this session — use HOBBES_TERMINAL_EXEC first.")?;

        let sentinel = session
            .running
            .lock()
            .map_err(|_| "state poisoned")?
            .as_ref()
            .map(|r| r.sentinel.clone());
        let Some(sentinel) = sentinel else {
            return Ok("No command is running; nothing new to read.".to_string());
        };
        match self
            .read_until_sentinel(&session, &sentinel, timeout_secs)
            .await?
        {
            Some((output, code)) => {
                *session.running.lock().map_err(|_| "state poisoned")? = None;
                Ok(format!(
                    "{}\n(command finished — exit code: {code})",
                    clean_output(&output, OUTPUT_CAP_BYTES)
                ))
            }
            None => Ok(
                "Still running. Use HOBBES_TERMINAL_TAIL again to keep reading, or \
                 HOBBES_TERMINAL_RESET to kill the shell."
                    .to_string(),
            ),
        }
    }

    /// Accumulate channel output until the sentinel or the deadline.
    /// `Ok(None)` = timed out (partial output stays in the accumulated buf —
    /// returned to the caller inside the timeout message flow via `pending`
    /// semantics folded into the running marker; v1 returns the guidance
    /// only, output arrives on the next TAIL).
    async fn read_until_sentinel(
        &self,
        session: &Arc<PtySession>,
        sentinel: &str,
        timeout_secs: u64,
    ) -> Result<Option<(String, i32)>, String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut buf: Vec<u8> = Vec::new();
        let mut rx = session.output_rx.lock().await;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(bytes)) => {
                    buf.extend_from_slice(&bytes);
                    if let Some(hit) = split_at_sentinel(&buf, sentinel) {
                        return Ok(Some(hit));
                    }
                }
                Ok(None) => {
                    return Err(
                        "The shell exited unexpectedly. HOBBES_TERMINAL_EXEC will start a \
                         fresh one."
                            .to_string(),
                    )
                }
                Err(_) => return Ok(None),
            }
        }
    }

    pub fn list_tools(&self) -> Vec<Tool> {
        use std::sync::Arc;
        vec![
            Tool {
                name: "HOBBES_TERMINAL_EXEC".into(),
                description: Some(
                    "Run a shell command in this chat's persistent terminal (zsh, login \
                     PATH). The shell survives between calls — cd, exports, and \
                     virtualenvs persist. Starts in the session's project directory. \
                     Returns output and exit code; long commands return partial state \
                     with guidance to HOBBES_TERMINAL_TAIL."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "The shell command to run." },
                            "timeout_secs": { "type": "integer", "description": "Seconds to wait before returning with the command still running (default 30, max 300)." },
                            "cwd": { "type": "string", "description": "Run in this directory via a subshell without changing the session's cwd." }
                        },
                        "required": ["command"]
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Run Terminal Command".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TERMINAL_TAIL".into(),
                description: Some(
                    "Read more output from a command that was still running when \
                     HOBBES_TERMINAL_EXEC returned. Reports the exit code once it \
                     finishes."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "timeout_secs": { "type": "integer", "description": "Seconds to wait for more output (default 5, max 60)." }
                        }
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Read Terminal Output".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
            Tool {
                name: "HOBBES_TERMINAL_RESET".into(),
                description: Some(
                    "Kill this chat's terminal shell (and any running command). The next \
                     exec starts a fresh shell in the project directory."
                        .into(),
                ),
                input_schema: Arc::new(
                    serde_json::from_value(serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }))
                    .unwrap_or_default(),
                ),
                title: Some("Reset Terminal".to_string()),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_parses_and_splits() {
        let s = "__HOBBES_DONE_abc__";
        let buf = b"hello\nworld\n\n__HOBBES_DONE_abc__ 0\n";
        let (out, code) = split_at_sentinel(buf, s).unwrap();
        assert_eq!(out, "hello\nworld\n\n");
        assert_eq!(code, 0);
        // Nonzero, and split across the accumulated buffer is fine (we scan
        // the whole accumulation each chunk).
        let buf2 = b"x __HOBBES_DONE_abc__ 127\n";
        assert_eq!(split_at_sentinel(buf2, s).unwrap().1, 127);
        assert!(split_at_sentinel(b"no sentinel here", s).is_none());
        assert!(split_at_sentinel(b"__HOBBES_DONE_abc__ nope", s).is_none());

        // The tty echoes the typed printf: the FIRST sentinel occurrence is
        // the format string (`%d`), the real one follows. Must skip to it.
        let echoed =
            b"printf '\\n__HOBBES_DONE_abc__ %d\\n' $?\r\nreal-output\n\n__HOBBES_DONE_abc__ 0\n";
        let (out, code) = split_at_sentinel(echoed, s).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("real-output"));
    }

    #[test]
    fn output_cleaning_strips_ansi_and_caps_tail() {
        let noisy = "\x1b[31mred\x1b[0m plain \x1b]0;title\x07end";
        assert_eq!(clean_output(noisy, 10_000), "red plain end");

        let long = format!("{}{}", "a".repeat(200_000), "TAIL_MARKER");
        let cleaned = clean_output(&long, 1024);
        assert!(cleaned.starts_with("[output truncated"));
        assert!(cleaned.ends_with("TAIL_MARKER"));
        assert!(cleaned.len() < 1200);
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/it's here"), r"'/tmp/it'\''s here'");
    }

    #[tokio::test]
    async fn exec_round_trip_persistence_and_reset() {
        let client = TerminalClient::new();
        let ctx = TerminalCtx {
            session_id: "t1".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        };
        let run = |cmd: &str| {
            let client = client.clone();
            let ctx = ctx.clone();
            let args = serde_json::json!({"command": cmd, "timeout_secs": 20});
            async move {
                client
                    .exec(&ctx, &args)
                    .await
                    .unwrap_or_else(|e| panic!("exec failed: {e}"))
            }
        };

        let out = run("echo hobbes-terminal-ok").await;
        assert!(out.contains("hobbes-terminal-ok"), "got: {out}");
        assert!(out.contains("(exit code: 0)"));

        // State persists across calls.
        run("cd /tmp && export HOBBES_T=42").await;
        let out = run("echo $HOBBES_T $(pwd)").await;
        assert!(out.contains("42"), "env persisted: {out}");
        assert!(out.contains("/tmp"), "cwd persisted: {out}");

        // Nonzero exit propagates.
        let out = run("false").await;
        assert!(out.contains("(exit code: 1)"), "got: {out}");

        // Reset kills; next exec respawns fresh (env gone).
        client.kill_session("t1").await;
        let out = run("echo ${HOBBES_T:-gone}").await;
        assert!(out.contains("gone"), "fresh shell after reset: {out}");

        client.shutdown_all().await;
        assert!(client.sessions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn timeout_then_tail_completes() {
        let client = TerminalClient::new();
        let ctx = TerminalCtx {
            session_id: "t2".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        };
        let args = serde_json::json!({"command": "sleep 2 && echo slow-done", "timeout_secs": 1});
        let out = client.exec(&ctx, &args).await.unwrap();
        assert!(out.contains("still running"), "got: {out}");

        // A second exec while running is refused with guidance.
        let err = client
            .exec(&ctx, &serde_json::json!({"command": "echo nope"}))
            .await
            .unwrap_err();
        assert!(err.contains("HOBBES_TERMINAL_TAIL"));

        let out = client
            .tail(&ctx, &serde_json::json!({"timeout_secs": 20}))
            .await
            .unwrap();
        assert!(out.contains("slow-done"), "got: {out}");
        assert!(out.contains("exit code: 0"));
        client.shutdown_all().await;
    }
}
