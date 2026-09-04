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
//! shell, feeding a CAPPED ring buffer ([`RING_CAP_BYTES`]); the async side
//! only ever awaits `tokio::time::timeout(_, notify.notified())`, so a hung
//! command can never wedge the turn (there is no global tool timeout
//! upstream). Every buffer on this path is bounded — the ring, the async
//! accumulator ([`ACC_CAP_BYTES`]), and the carry saved across a timeout —
//! because the MCP dispatch awaits on the Dioxus main-thread executor: a
//! chatty long-running command must cost O(cap) memory and per-chunk CPU,
//! never O(total output). The reader thread holds only the ring Arc — never
//! the session Arc — so Drop runs.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use portable_pty::{CommandBuilder, PtySize};
use rmcp::model::Tool;

pub const HOBBES_TERMINAL_SERVER: &str = "hobbes-terminal";

/// Output kept per command (the tail survives; a marker notes truncation).
const OUTPUT_CAP_BYTES: usize = 100 * 1024;
/// Reader-side ring cap: bounds memory while NOBODY is reading (a command
/// printing for minutes between TAIL calls). Must comfortably exceed the
/// accumulator cap so the sentinel tail always survives.
const RING_CAP_BYTES: usize = 512 * 1024;
/// Async-side accumulator cap: bounds the per-chunk sentinel scan (which
/// walks the whole accumulation) and the final `clean_output` input. Output
/// cap plus slack so the truncation marker, not the trim, decides display.
const ACC_CAP_BYTES: usize = OUTPUT_CAP_BYTES + 8 * 1024;
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
    /// Output already drained from the ring when a read timed out — seeds the
    /// next TAIL's accumulator so partial output isn't silently dropped.
    carry: Vec<u8>,
    /// The carry was front-trimmed at some point — survives across TAILs so
    /// the eventual output is marked truncated.
    carry_trimmed: bool,
}

/// Reader-thread → async-side handoff: a capped byte ring plus a wake
/// signal. The reader appends and trims under the (never-awaited) lock.
#[derive(Default)]
struct OutputRing {
    buf: Vec<u8>,
    closed: bool,
}

struct PtySession {
    writer: std::sync::Mutex<Box<dyn Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Kept alive for the pty's lifetime; dropping it hangs up the shell.
    _master: std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    ring: Arc<(std::sync::Mutex<OutputRing>, tokio::sync::Notify)>,
    running: std::sync::Mutex<Option<RunningCmd>>,
}

/// One `read_until_sentinel` outcome: the command finished, or the deadline
/// hit first (the drained-so-far bytes ride along as the carry). The bool is
/// "the accumulator was front-trimmed": display length alone can't detect it
/// (ANSI/CR stripping shrinks the text back under the cap), so the trim
/// reports itself.
enum ReadOutcome {
    Done(String, i32, bool),
    Pending(Vec<u8>, bool),
}

/// Keep only the last `cap` bytes; reports whether anything was dropped.
fn trim_front(buf: &mut Vec<u8>, cap: usize) -> bool {
    if buf.len() > cap {
        buf.drain(..buf.len() - cap);
        true
    } else {
        false
    }
}

/// Display shaping shared by EXEC and TAIL: clean, cap, and make sure a
/// front-trimmed accumulation is marked even when the cleaned text no longer
/// exceeds the display cap.
fn shape_output(output: &str, was_trimmed: bool) -> String {
    let cleaned = clean_output(output, OUTPUT_CAP_BYTES);
    if was_trimmed && !cleaned.starts_with("[output truncated") {
        format!("[output truncated — showing the tail]\n{cleaned}")
    } else {
        cleaned
    }
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

    let ring = Arc::new((
        std::sync::Mutex::new(OutputRing::default()),
        tokio::sync::Notify::new(),
    ));
    let ring_writer = ring.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => {
                    let (lock, notify) = &*ring_writer;
                    if let Ok(mut r) = lock.lock() {
                        r.closed = true;
                    }
                    notify.notify_one();
                    break;
                }
                Ok(n) => {
                    let (lock, notify) = &*ring_writer;
                    let Ok(mut r) = lock.lock() else { break };
                    r.buf.extend_from_slice(&buf[..n]);
                    // The cap bounds idle-time growth; a command's middle can
                    // be lost, its tail (where the sentinel lands) cannot.
                    let _ = trim_front(&mut r.buf, RING_CAP_BYTES);
                    drop(r);
                    notify.notify_one();
                }
            }
        }
    });

    let session = PtySession {
        writer: std::sync::Mutex::new(writer),
        child: std::sync::Mutex::new(child),
        _master: std::sync::Mutex::new(pair.master),
        ring,
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
            .read_until_sentinel(&session, &sentinel, timeout_secs, Vec::new(), false)
            .await?
        {
            ReadOutcome::Done(output, code, trimmed) => {
                Ok(format!("{}\n(exit code: {code})", shape_output(&output, trimmed)))
            }
            ReadOutcome::Pending(carry, carry_trimmed) => {
                *session.running.lock().map_err(|_| "state poisoned")? = Some(RunningCmd {
                    sentinel,
                    command: command.to_string(),
                    carry,
                    carry_trimmed,
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

        // Take the carry out (rather than clone) so a re-timeout replaces it
        // instead of duplicating what this read already drained.
        let taken = session
            .running
            .lock()
            .map_err(|_| "state poisoned")?
            .as_mut()
            .map(|r| {
                (
                    r.sentinel.clone(),
                    std::mem::take(&mut r.carry),
                    r.carry_trimmed,
                )
            });
        let Some((sentinel, carry, carry_trimmed)) = taken else {
            return Ok("No command is running; nothing new to read.".to_string());
        };
        match self
            .read_until_sentinel(&session, &sentinel, timeout_secs, carry, carry_trimmed)
            .await?
        {
            ReadOutcome::Done(output, code, trimmed) => {
                *session.running.lock().map_err(|_| "state poisoned")? = None;
                Ok(format!(
                    "{}\n(command finished — exit code: {code})",
                    shape_output(&output, trimmed)
                ))
            }
            ReadOutcome::Pending(carry, carry_trimmed) => {
                if let Some(r) = session
                    .running
                    .lock()
                    .map_err(|_| "state poisoned")?
                    .as_mut()
                {
                    r.carry = carry;
                    r.carry_trimmed = carry_trimmed;
                }
                Ok(
                    "Still running. Use HOBBES_TERMINAL_TAIL again to keep reading, or \
                     HOBBES_TERMINAL_RESET to kill the shell."
                        .to_string(),
                )
            }
        }
    }

    /// Drain the ring into a CAPPED accumulator until the sentinel or the
    /// deadline. `Pending` carries the drained-so-far bytes so the caller can
    /// stash them on the running marker — the next TAIL seeds from it and no
    /// partial output is lost. Every structure here is bounded: the sentinel
    /// scan and the eventual `clean_output` see at most [`ACC_CAP_BYTES`],
    /// however much the command prints.
    async fn read_until_sentinel(
        &self,
        session: &Arc<PtySession>,
        sentinel: &str,
        timeout_secs: u64,
        mut acc: Vec<u8>,
        mut trimmed: bool,
    ) -> Result<ReadOutcome, String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let (lock, notify) = &*session.ring;
        loop {
            let (chunk, closed) = {
                let mut r = lock.lock().map_err(|_| "pty ring poisoned")?;
                (std::mem::take(&mut r.buf), r.closed)
            };
            if !chunk.is_empty() {
                acc.extend_from_slice(&chunk);
                trimmed |= trim_front(&mut acc, ACC_CAP_BYTES);
                if let Some(hit) = split_at_sentinel(&acc, sentinel) {
                    return Ok(ReadOutcome::Done(hit.0, hit.1, trimmed));
                }
            }
            if closed {
                return Err(
                    "The shell exited unexpectedly. HOBBES_TERMINAL_EXEC will start a \
                     fresh one."
                        .to_string(),
                );
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(ReadOutcome::Pending(acc, trimmed));
            }
            // A wake lost to the drain-then-wait race is caught by the next
            // loop's drain (Notify stores one permit).
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
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
        let args = serde_json::json!({
            "command": "echo early-part && sleep 2 && echo slow-done",
            "timeout_secs": 1
        });
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
        assert!(
            out.contains("early-part"),
            "pre-timeout output survives via the carry: {out}"
        );
        assert!(out.contains("slow-done"), "got: {out}");
        assert!(out.contains("exit code: 0"));
        client.shutdown_all().await;
    }

    #[tokio::test]
    async fn chatty_output_stays_bounded_and_completes() {
        let client = TerminalClient::new();
        let ctx = TerminalCtx {
            session_id: "t3".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        };
        // ~2MB of output — far past every cap. Must finish promptly (the
        // per-chunk sentinel scan is bounded by the accumulator cap, not the
        // total printed) and come back truncated to the tail.
        let args = serde_json::json!({"command": "seq 1 300000", "timeout_secs": 60});
        let out = client.exec(&ctx, &args).await.unwrap();
        assert!(out.contains("[output truncated"), "got head: {}", &out[..200.min(out.len())]);
        assert!(out.contains("300000"), "tail survives");
        assert!(out.contains("(exit code: 0)"));
        client.shutdown_all().await;
    }

    #[test]
    fn trim_front_keeps_the_tail() {
        let mut b: Vec<u8> = (0u8..=255).collect();
        assert!(trim_front(&mut b, 16));
        assert_eq!(b.len(), 16);
        assert_eq!(*b.last().unwrap(), 255);
        let mut small = vec![1u8, 2];
        assert!(!trim_front(&mut small, 16));
        assert_eq!(small, vec![1, 2]);
    }
}
