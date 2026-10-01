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
//!
//! Output attribution: EXEC/TAIL on one shell are serialized (`op_lock`),
//! and EXEC claims the `running` marker before writing, so a timed-out or
//! abandoned command's output is only ever returned by TAIL (or discarded by
//! RESET) — never mixed into a later EXEC. As defence in depth, finalized
//! output drops anything before another command's completed sentinel and
//! scrubs any protocol sentinel text before it reaches the model.

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

/// Appended to PATH when present and missing: `zsh -f` skips ~/.zshrc, which
/// is where rustup/cargo, pipx and Homebrew usually land, so a login-only PATH
/// misses `cargo`, `claude`, etc.
const PATH_FIXUP: &str = "for d in \"$HOME/.cargo/bin\" \"$HOME/.local/bin\" /opt/homebrew/bin /opt/homebrew/sbin /usr/local/bin; do \
[ -d \"$d\" ] && case \":$PATH:\" in *\":$d:\"*) ;; *) PATH=\"$PATH:$d\";; esac; done; export PATH;";

/// `hobbes_detach LOGFILE cmd args…` — start a fully detached process: new
/// session (no controlling tty, so no SIGHUP when the shell or pty goes away
/// and `/dev/tty` opens fail), own process group, stdin from /dev/null,
/// stdout+stderr appended to LOGFILE. macOS has no `setsid(1)`, so perl does it.
const DETACH_FN: &str = "hobbes_detach() { local _log=\"$1\"; shift; \
perl -MPOSIX -e 'exit 0 if fork; POSIX::setsid(); exit 0 if fork; exec @ARGV or exit 127' \"$@\" </dev/null >>\"$_log\" 2>&1 & disown 2>/dev/null; };";

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
    /// The exact command text written to the pty (post cwd-wrap) — needed at
    /// TAIL time to strip an echoed copy from the front of the output.
    wrapped: String,
    /// The exact `printf '...sentinel... %d\n' $?` line written to the pty —
    /// same reason as `wrapped`.
    sentinel_line: String,
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
    /// Serializes EXEC/TAIL on this shell. Every tool call runs in its own
    /// spawned task, so two EXECs from one assistant turn would otherwise
    /// both pass the `running` check, both write, and both drain the SAME
    /// ring — each result then carries the other command's output and raw
    /// sentinel. Held across the (bounded) read, so a waiter waits at most
    /// one call's timeout.
    op_lock: tokio::sync::Mutex<()>,
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

/// Full response-shaping pipeline: drop anything left over from earlier
/// commands, strip an echoed copy of the input, strip stray continuation
/// prompts, then the ANSI-strip/cap/truncate shaping, and finally scrub any
/// protocol sentinel that survived. Shared by EXEC's and TAIL's `Done` arms —
/// echoed input always lands at the very front of the accumulation
/// regardless of which call finalizes it.
fn finalize_output(output: &str, wrapped: &str, sentinel_line: &str, was_trimmed: bool) -> String {
    let own = drop_foreign_sentinel_prefix(output);
    let de_echoed = strip_echoed_input(own, wrapped, sentinel_line);
    let de_prompted = strip_continuation_prompts(&de_echoed);
    scrub_sentinels(&shape_output(&de_prompted, was_trimmed))
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

/// If `s` starts with ` <int>\r?\n` — the tail of a COMPLETE sentinel line —
/// return (exit code, bytes consumed). A line still arriving (`__ 1` of an
/// eventual `__ 127`) must not match, or the code is wrong and the rest of
/// the line leaks into the next command's output.
fn parse_sentinel_code(s: &str) -> Option<(i32, usize)> {
    let rest = s.strip_prefix(' ')?;
    let digits_start = usize::from(rest.starts_with('-'));
    let digits_len = rest[digits_start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits_len == 0 {
        return None;
    }
    let num_end = digits_start + digits_len;
    let code = rest[..num_end].parse::<i32>().ok()?;
    let after = &rest[num_end..];
    let eol = if after.starts_with("\r\n") {
        2
    } else if after.starts_with('\n') {
        1
    } else {
        return None;
    };
    Some((code, 1 + num_end + eol))
}

/// Find `__HOBBES_DONE_<nonce>__ <code>\n` in the accumulated bytes; return
/// (output before it, exit code). The tty ECHOES the typed command, so the
/// first occurrence is usually the echoed printf format (followed by `%d`,
/// not a number) — scan every occurrence until one parses as a complete line.
pub(crate) fn split_at_sentinel(buf: &[u8], sentinel: &str) -> Option<(String, i32)> {
    let text = String::from_utf8_lossy(buf);
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(sentinel) {
        let idx = search + rel;
        if let Some((code, _)) = parse_sentinel_code(&text[idx + sentinel.len()..]) {
            return Some((text[..idx].to_string(), code));
        }
        search = idx + sentinel.len();
    }
    None
}

/// Locate the next protocol sentinel token at or after `from`:
/// `__HOBBES_DONE_<32 hex>__` or `__HOBBES_INIT_<32 hex>__` (the nonce is a
/// simple-format UUID). Returns its byte range. Requiring the full hex nonce
/// keeps source code that merely mentions the format (`__HOBBES_DONE_{nonce}__`)
/// from matching.
fn find_sentinel_token(s: &str, from: usize) -> Option<(usize, usize)> {
    const NONCE_LEN: usize = 32;
    let mut search = from;
    while let Some(rel) = s[search..].find("__HOBBES_") {
        let start = search + rel;
        let after_prefix = &s[start + "__HOBBES_".len()..];
        if after_prefix.starts_with("DONE_") || after_prefix.starts_with("INIT_") {
            let nonce = &after_prefix.as_bytes()["DONE_".len()..];
            if nonce.len() >= NONCE_LEN + 2
                && nonce[..NONCE_LEN]
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
                && &nonce[NONCE_LEN..NONCE_LEN + 2] == b"__"
            {
                let end = start + "__HOBBES_DONE_".len() + NONCE_LEN + 2;
                return Some((start, end));
            }
        }
        search = start + "__HOBBES_".len();
    }
    None
}

/// Drop everything up to and including the LAST completed sentinel line of
/// ANOTHER command (or the init line) in a finished command's output. The
/// shell runs lines strictly in order, so anything before a completed foreign
/// sentinel belongs to earlier work — late init residue when the bounded init
/// wait gave up, or a command whose reader went away — never to this one.
/// Only called on text that precedes this command's own sentinel, so its own
/// line can't be matched here (and its echoed printf has `%d`, not a code).
pub(crate) fn drop_foreign_sentinel_prefix(output: &str) -> &str {
    let mut cut = None;
    let mut search = 0usize;
    while let Some((_, end)) = find_sentinel_token(output, search) {
        if let Some((_, consumed)) = parse_sentinel_code(&output[end..]) {
            cut = Some(end + consumed);
        }
        search = end;
    }
    cut.map_or(output, |c| &output[c..])
}

/// Last line of defence: no raw protocol sentinel may ever reach the model.
/// A line that is nothing but a sentinel (`__HOBBES_DONE_… 0`) or an echoed
/// sentinel `printf` is removed outright; any other occurrence has just the
/// token replaced, so a genuine output line around it survives.
pub(crate) fn scrub_sentinels(text: &str) -> String {
    if find_sentinel_token(text, 0).is_none() {
        return text.to_string();
    }
    let mut kept: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let Some((start, end)) = find_sentinel_token(line, 0) else {
            kept.push(line.to_string());
            continue;
        };
        let before = line[..start].trim();
        let after = line[end..].trim();
        let bare = before.is_empty()
            && (after.is_empty() || after.trim_start_matches('-').parse::<u32>().is_ok());
        let echoed_printf = before.starts_with("printf ");
        if bare || echoed_printf {
            continue;
        }
        let mut cleaned = String::with_capacity(line.len());
        let mut pos = 0usize;
        while let Some((s, e)) = find_sentinel_token(line, pos) {
            cleaned.push_str(&line[pos..s]);
            cleaned.push_str("[hobbes-sentinel]");
            pos = e;
        }
        cleaned.push_str(&line[pos..]);
        kept.push(cleaned);
    }
    kept.join("\n")
}

/// Render raw pty output to plain text the way a terminal would display it:
/// strip escape sequences (CSI, OSC, DCS/SOS/PM/APC strings, charset and
/// other two-byte escapes) while KEEPING tabs and newlines. `\r` returns to
/// column 0 and later text overwrites (so progress bars collapse to their
/// final state), backspace steps back, and `ESC[K` erases. Every other
/// control character is dropped.
///
/// Replaces `strip_ansi_escapes::strip_str`, whose vte-based parser executes
/// C0 controls without emitting them — it silently deleted `\t`, turning
/// `printf 'A\tB'` into `AB` and gluing `ls` columns together.
pub(crate) fn strip_terminal_controls(raw: &str) -> String {
    fn put(line: &mut Vec<char>, col: &mut usize, c: char) {
        while line.len() < *col {
            line.push(' ');
        }
        if *col < line.len() {
            line[*col] = c;
        } else {
            line.push(c);
        }
        *col += 1;
    }

    let mut out = String::with_capacity(raw.len());
    let mut line: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        let csi = match c {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    true
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    // String sequence: runs to BEL or ST (`ESC \`).
                    chars.next();
                    while let Some(p) = chars.next() {
                        if p == '\u{07}' {
                            break;
                        }
                        if p == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                    continue;
                }
                Some(p) if ('\u{20}'..='\u{2f}').contains(&p) => {
                    // nF escape (e.g. `ESC ( B`): intermediates then a final.
                    while chars.next_if(|p| ('\u{20}'..='\u{2f}').contains(p)).is_some() {}
                    chars.next();
                    continue;
                }
                Some(p) if ('\u{30}'..='\u{7e}').contains(&p) => {
                    // Two-byte escape (`ESC =`, `ESC 7`, `ESC M`, …).
                    chars.next();
                    continue;
                }
                _ => continue,
            },
            '\u{9b}' => true,
            _ => false,
        };
        if csi {
            // Parameter/intermediate bytes, then a final byte. A byte outside
            // the CSI ranges aborts the sequence and is processed normally,
            // so a malformed escape can't swallow real text or a newline.
            let mut params = String::new();
            let mut fin = None;
            while let Some(&p) = chars.peek() {
                if ('\u{20}'..='\u{3f}').contains(&p) {
                    params.push(p);
                    chars.next();
                } else {
                    if ('\u{40}'..='\u{7e}').contains(&p) {
                        fin = Some(p);
                        chars.next();
                    }
                    break;
                }
            }
            if fin == Some('K') {
                match params.as_str() {
                    "" | "0" => line.truncate(col),
                    "1" => {
                        for slot in line.iter_mut().take(col + 1) {
                            *slot = ' ';
                        }
                    }
                    "2" => line.clear(),
                    _ => {}
                }
            }
            continue;
        }
        match c {
            '\n' => {
                out.extend(line.drain(..));
                out.push('\n');
                col = 0;
            }
            '\r' => col = 0,
            '\u{08}' => col = col.saturating_sub(1),
            '\t' => put(&mut line, &mut col, c),
            c if c.is_control() => {}
            c => put(&mut line, &mut col, c),
        }
    }
    out.extend(line);
    out
}

/// Strip terminal controls and keep the LAST `cap` bytes (UTF-8-safe),
/// marking truncation.
pub(crate) fn clean_output(raw: &str, cap: usize) -> String {
    let stripped = strip_terminal_controls(raw);
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

/// Strip an echoed copy of what we TYPED from the front of the captured
/// output. `stty -echo` is sent on shell spawn but cannot be relied upon (the
/// setting may not have taken effect yet, or the pty may re-enable it), so a
/// tty in echo mode reproduces every line we wrote — the wrapped command
/// (which may itself span multiple lines) followed by the sentinel `printf`
/// line — before any real output appears.
///
/// Compares line-by-line, in order, against the lines we know we sent
/// (`wrapped_command`'s lines, then `sentinel_line`), stopping at the first
/// output line that doesn't match. This bounds how much can ever be
/// stripped to exactly the number of lines we sent, so real command output
/// that happens to start with the same text as the command itself is never
/// over-stripped.
pub(crate) fn strip_echoed_input(output: &str, wrapped_command: &str, sentinel_line: &str) -> String {
    let mut expected = wrapped_command.lines().collect::<Vec<_>>();
    expected.push(sentinel_line);

    let mut rest = output;
    for exp in expected {
        let exp = exp.trim_end_matches('\r');
        match rest.find('\n') {
            Some(nl) => {
                let line = rest[..nl].trim_end_matches('\r');
                if line != exp {
                    break;
                }
                rest = &rest[nl + 1..];
            }
            None => {
                // Last (partial) line in the buffer with no trailing newline
                // yet — only consume it if it exactly matches; otherwise it
                // is real (still-buffering) output, not an echo.
                if rest.trim_end_matches('\r') == exp {
                    rest = "";
                }
                break;
            }
        }
    }
    rest.to_string()
}

/// zsh continuation-prompt tokens (`PS2`/`PS3`/`PS4` variants) that leak into
/// the capture when a multi-line command isn't read as a single unit by the
/// shell (an unterminated quote, a `for`/`while`/`if` split across lines,
/// etc). Strips leading runs of `token> ` at the START of each line only —
/// text elsewhere in a line (e.g. legitimate output containing "dquote> ")
/// is left untouched.
const CONTINUATION_TOKENS: &[&str] = &[
    "cmdand", "dquote", "quote", "cmdsubst", "heredoc", "pipe", "then", "else", "for", "while",
    "if", "braceparam", "bquote",
];

pub(crate) fn strip_continuation_prompts(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rest = line;
        loop {
            let stripped = CONTINUATION_TOKENS.iter().find_map(|tok| {
                let prefix = format!("{tok}> ");
                rest.strip_prefix(prefix.as_str())
            });
            match stripped {
                Some(next) => rest = next,
                None => break,
            }
        }
        out.push_str(rest);
    }
    out
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

fn spawn_shell(cwd: &str, init_sentinel: &str) -> Result<PtySession, String> {
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
        op_lock: tokio::sync::Mutex::new(()),
    };

    // Neuter prompts/echo so the sentinel protocol sees clean output, then
    // recover the login PATH the -f flag skipped. PS2/PS3/PS4 must also be
    // cleared — otherwise a multi-line command (an unterminated quote spread
    // across lines, a for/while split up) prints zsh's secondary/select/xtrace
    // prompt (`cmdand> `, `dquote> `, ...) into the capture on every
    // continuation line. Echo (`stty -echo`) is not guaranteed to have taken
    // effect for everything that follows, so the init line ends with its own
    // sentinel: `session_for` waits (bounded) for it and discards everything
    // up to and including it, so init residue (and its own echo, if any)
    // never reaches a response.
    session.write_line(&format!(
        "unsetopt ZLE PROMPT_SP PROMPT_CR 2>/dev/null; PS1=''; PROMPT=''; RPROMPT=''; \
         PS2=''; PS3=''; PS4=''; \
         stty -echo 2>/dev/null; export TERM=dumb; \
         [ -f /etc/zprofile ] && source /etc/zprofile >/dev/null 2>&1; \
         [ -f \"$HOME/.zprofile\" ] && source \"$HOME/.zprofile\" >/dev/null 2>&1; \
         {PATH_FIXUP} \
         {DETACH_FN} \
         printf '\\n{init_sentinel} %d\\n' $?"
    ))?;
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
        let init_nonce = uuid::Uuid::new_v4().simple().to_string();
        let init_sentinel = format!("__HOBBES_INIT_{init_nonce}__");
        let fresh = Arc::new(spawn_shell(&ctx.cwd, &init_sentinel)?);
        map.insert(ctx.session_id.clone(), fresh.clone());
        // Wait (bounded) for the init line's own sentinel and discard
        // everything up to and including it — init output (zprofile sourcing,
        // any stray echo before `stty -echo` takes effect) must never reach a
        // response. Best-effort: if the shell is unusually slow, or dies, the
        // normal EXEC path below will surface the problem, and late init
        // residue is cut by `drop_foreign_sentinel_prefix` at finalize time.
        let _ = self
            .read_until_sentinel(&fresh, &init_sentinel, 2, Vec::new(), false)
            .await;
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
        let _op = session.op_lock.lock().await;

        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let sentinel = format!("__HOBBES_DONE_{nonce}__");
        let wrapped = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(dir) if !dir.trim().is_empty() => {
                format!("( cd {} && {} )", shell_quote(dir.trim()), command)
            }
            _ => command.to_string(),
        };
        let sentinel_line = format!("printf '\\n{sentinel} %d\\n' $?");

        // Claim the shell BEFORE writing: if this future is dropped mid-read
        // (turn cancelled), the marker stays, so the next EXEC is refused and
        // the orphaned output is only ever reachable via TAIL — it can never
        // be drained into a later command's result.
        {
            let mut running = session.running.lock().map_err(|_| "state poisoned")?;
            if let Some(r) = &*running {
                return Err(format!(
                    "A command is still running in this shell: `{}`. Use \
                     HOBBES_TERMINAL_TAIL to read its output, or \
                     HOBBES_TERMINAL_RESET to kill the shell.",
                    r.command
                ));
            }
            *running = Some(RunningCmd {
                sentinel: sentinel.clone(),
                command: command.to_string(),
                wrapped: wrapped.clone(),
                sentinel_line: sentinel_line.clone(),
                carry: Vec::new(),
                carry_trimmed: false,
            });
        }
        if let Err(e) = session.write_line(&format!("{wrapped}\n{sentinel_line}")) {
            *session.running.lock().map_err(|_| "state poisoned")? = None;
            return Err(e);
        }

        match self
            .read_until_sentinel(&session, &sentinel, timeout_secs, Vec::new(), false)
            .await?
        {
            ReadOutcome::Done(output, code, trimmed) => {
                *session.running.lock().map_err(|_| "state poisoned")? = None;
                Ok(format!(
                    "{}\n(exit code: {code})",
                    finalize_output(&output, &wrapped, &sentinel_line, trimmed)
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

        let _op = session.op_lock.lock().await;
        // Clone (not take) the carry: a re-timeout REPLACES it with this
        // read's superset, so nothing duplicates, and if this future is
        // dropped mid-read the stored carry is still intact.
        let taken = session
            .running
            .lock()
            .map_err(|_| "state poisoned")?
            .as_ref()
            .map(|r| {
                (
                    r.sentinel.clone(),
                    r.wrapped.clone(),
                    r.sentinel_line.clone(),
                    r.carry.clone(),
                    r.carry_trimmed,
                )
            });
        let Some((sentinel, wrapped, sentinel_line, carry, carry_trimmed)) = taken else {
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
                    finalize_output(&output, &wrapped, &sentinel_line, trimmed)
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
                     with guidance to HOBBES_TERMINAL_TAIL. To start a long-lived background \
                     process that must outlive the shell and HOBBES_TERMINAL_RESET, use \
                     `hobbes_detach LOGFILE cmd args…` (own session, no tty, stdin from \
                     /dev/null) instead of `nohup … &`."
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

    #[test]
    fn strip_echoed_input_removes_full_echo() {
        let wrapped = "echo hi";
        let sentinel_line = "printf '\\n__HOBBES_DONE_x__ %d\\n' $?";
        let output = format!("{wrapped}\r\n{sentinel_line}\r\nhi\n\n");
        assert_eq!(strip_echoed_input(&output, wrapped, sentinel_line), "hi\n\n");
    }

    #[test]
    fn strip_echoed_input_multiline_command_echo() {
        let wrapped = "python3 -c \"\nprint(1)\nprint(2)\n\"";
        let sentinel_line = "printf '\\n__HOBBES_DONE_y__ %d\\n' $?";
        let output = format!("{wrapped}\n{sentinel_line}\n1\n2\n");
        assert_eq!(strip_echoed_input(&output, wrapped, sentinel_line), "1\n2\n");
    }

    #[test]
    fn strip_echoed_input_no_echo_is_noop() {
        let wrapped = "echo hi";
        let sentinel_line = "printf '\\n__HOBBES_DONE_z__ %d\\n' $?";
        let output = "hi\n\n";
        assert_eq!(strip_echoed_input(output, wrapped, sentinel_line), output);
    }

    #[test]
    fn strip_echoed_input_partial_echo_stops_at_mismatch() {
        // Only the command line echoed (e.g. echo was disabled mid-write, or
        // the printf sentinel line was never echoed because -echo landed by
        // then) — must stop cleanly at the first non-matching line rather
        // than eating real output.
        let wrapped = "echo hi";
        let sentinel_line = "printf '\\n__HOBBES_DONE_w__ %d\\n' $?";
        let output = format!("{wrapped}\nhi\n\n");
        assert_eq!(strip_echoed_input(&output, wrapped, sentinel_line), "hi\n\n");
    }

    #[test]
    fn strip_echoed_input_never_over_strips_legitimate_output() {
        // A line of genuine output that happens to coincide with a line we
        // sent is indistinguishable from an echo by construction, so it is
        // still eligible to match — but the very next expected line
        // (sentinel_line) does NOT match here, so stripping must stop right
        // there: exactly one line consumed, everything after (including a
        // second "more" line that has nothing to do with what we sent) must
        // survive untouched.
        let wrapped = "echo 'echo hi'";
        let sentinel_line = "printf '\\n__HOBBES_DONE_v__ %d\\n' $?";
        let output = format!("{wrapped}\nmore\n");
        assert_eq!(
            strip_echoed_input(&output, wrapped, sentinel_line),
            "more\n",
            "only the single coincidentally-matching line may be stripped, never beyond it"
        );
    }

    #[test]
    fn strip_continuation_prompts_removes_leading_tokens_only() {
        let s = "dquote> print(1)\ncmdand> print(2)\nnormal line\nnested: dquote> not at start";
        assert_eq!(
            strip_continuation_prompts(s),
            "print(1)\nprint(2)\nnormal line\nnested: dquote> not at start"
        );
    }

    #[test]
    fn strip_continuation_prompts_strips_multiple_stacked_tokens() {
        let s = "heredoc> dquote> stacked line";
        assert_eq!(strip_continuation_prompts(s), "stacked line");
    }

    #[test]
    fn strip_continuation_prompts_leaves_clean_text_alone() {
        let s = "just some\nnormal output\nno prompts here";
        assert_eq!(strip_continuation_prompts(s), s);
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
    async fn multiline_command_produces_clean_output() {
        // A real PTY exercise of items (a)-(c): a multi-line command (an
        // unterminated-looking `python3 -c "..."` spanning several lines)
        // must not leak zsh's PS2 continuation prompt or an echoed copy of
        // the command into the captured output.
        let client = TerminalClient::new();
        let ctx = TerminalCtx {
            session_id: "t4".into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        };
        let command = "python3 -c \"\nprint('line-one')\nprint('line-two')\n\"";
        let out = client
            .exec(&ctx, &serde_json::json!({"command": command, "timeout_secs": 20}))
            .await
            .unwrap_or_else(|e| panic!("exec failed: {e}"));

        assert!(out.contains("line-one"), "got: {out}");
        assert!(out.contains("line-two"), "got: {out}");
        assert!(out.contains("(exit code: 0)"), "got: {out}");
        assert!(
            !out.contains("python3 -c"),
            "echoed command text leaked into output: {out}"
        );
        assert!(
            !out.contains("dquote>") && !out.contains("cmdand>"),
            "zsh continuation prompt leaked into output: {out}"
        );
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

    const NONCE_A: &str = "0123456789abcdef0123456789abcdef";
    const NONCE_B: &str = "fedcba9876543210fedcba9876543210";

    #[test]
    fn sentinel_requires_a_complete_line() {
        let s = "__HOBBES_DONE_abc__";
        // `127` still arriving: `__ 1` must NOT finalize as exit code 1 (and
        // leave `27` to leak into the next command).
        assert!(split_at_sentinel(b"out\n__HOBBES_DONE_abc__ 1", s).is_none());
        assert!(split_at_sentinel(b"out\n__HOBBES_DONE_abc__ 12\r", s).is_none());
        let (out, code) = split_at_sentinel(b"out\n__HOBBES_DONE_abc__ 127\r\n", s).unwrap();
        assert_eq!((out.as_str(), code), ("out\n", 127));
        assert_eq!(split_at_sentinel(b"__HOBBES_DONE_abc__ -1\n", s).unwrap().1, -1);
    }

    #[test]
    fn clean_output_preserves_tabs() {
        assert_eq!(clean_output("A\tB\tC\r\n", 10_000), "A\tB\tC");
        assert_eq!(
            clean_output(".gitignore:3:target\tsrc/target\r\n", 10_000),
            ".gitignore:3:target\tsrc/target"
        );
        assert_eq!(
            clean_output("async_persist.rs\tkeychain_tests.rs\r\n", 10_000),
            "async_persist.rs\tkeychain_tests.rs"
        );
    }

    #[test]
    fn clean_output_strips_escape_sequences() {
        // SGR colours, including 256-colour and bold.
        assert_eq!(
            clean_output("\x1b[1;38;5;208mwarn\x1b[0m: \x1b[32mok\x1b[m", 10_000),
            "warn: ok"
        );
        // OSC window title, BEL- and ST-terminated.
        assert_eq!(clean_output("\x1b]0;title\x07a\x1b]2;t2\x1b\\b", 10_000), "ab");
        // OSC 8 hyperlink.
        assert_eq!(
            clean_output("\x1b]8;;https://x.y\x1b\\link\x1b]8;;\x1b\\", 10_000),
            "link"
        );
        // Charset select, keypad mode, cursor save, 8-bit CSI.
        assert_eq!(clean_output("\x1b(Ba\x1b=b\x1b7c\u{9b}31md", 10_000), "abcd");
        // DEC private modes (cursor hide/show).
        assert_eq!(clean_output("\x1b[?25lx\x1b[?25h", 10_000), "x");
        // Stray BEL / NUL dropped; a malformed CSI can't eat the newline.
        assert_eq!(clean_output("a\x07b\x00c\x1b[12\nnext", 10_000), "abc\nnext");
    }

    #[test]
    fn clean_output_handles_carriage_returns() {
        // CRLF line endings.
        assert_eq!(clean_output("one\r\ntwo\r\n", 10_000), "one\ntwo");
        // Progress bar redraws collapse to the final state.
        assert_eq!(clean_output(" 10%\r 50%\r100%\r\ndone", 10_000), "100%\ndone");
        // Overwrite is column-wise like a terminal; ESC[K erases the rest.
        assert_eq!(clean_output("hello\rab", 10_000), "abllo");
        assert_eq!(clean_output("Downloading...\r\x1b[KDone\r\n", 10_000), "Done");
        assert_eq!(clean_output("xxxx\r\x1b[2Kyy", 10_000), "yy");
        // Backspace.
        assert_eq!(clean_output("ab\x08c", 10_000), "ac");
        // CR CR LF (a program that emits \r\n itself through onlcr).
        assert_eq!(clean_output("a\r\r\nb", 10_000), "a\nb");
    }

    #[test]
    fn clean_output_keeps_utf8() {
        assert_eq!(
            clean_output("\x1b[33mcafé 🐯 日本\x1b[0m\tñ\r\n", 10_000),
            "café 🐯 日本\tñ"
        );
        // Overwriting multi-byte chars works per character, not per byte.
        assert_eq!(clean_output("日本語\rab", 10_000), "ab語");
        // Truncation stays on a char boundary.
        let long = "é".repeat(2_000);
        let cleaned = clean_output(&long, 1025);
        assert!(cleaned.starts_with("[output truncated"));
        assert!(cleaned.ends_with('é'));
    }

    #[test]
    fn foreign_sentinel_prefix_is_dropped() {
        let stale = format!(
            "old-output\r\n\r\n__HOBBES_DONE_{NONCE_A}__ 0\r\n\r\n__HOBBES_INIT_{NONCE_B}__ 0\r\nmine\r\n"
        );
        assert_eq!(drop_foreign_sentinel_prefix(&stale), "mine\r\n");
        // An echoed printf (`%d`, not a code) is not a completed sentinel.
        let echoed = format!("printf '\\n__HOBBES_DONE_{NONCE_A}__ %d\\n' $?\r\nmine\n");
        assert_eq!(drop_foreign_sentinel_prefix(&echoed), echoed);
        // Short/non-hex nonces (source code mentioning the format) don't count.
        let code = "let s = \"__HOBBES_DONE_{nonce}__ 0\n\";\nrest";
        assert_eq!(drop_foreign_sentinel_prefix(code), code);
    }

    #[test]
    fn scrub_removes_every_sentinel() {
        let text = format!(
            "a\n__HOBBES_DONE_{NONCE_A}__ 0\nprintf '\\n__HOBBES_DONE_{NONCE_B}__ %d\\n' $?\n\
             log: saw __HOBBES_INIT_{NONCE_B}__ here\n__HOBBES_DONE_{NONCE_B}__\nb"
        );
        let scrubbed = scrub_sentinels(&text);
        assert!(!scrubbed.contains("__HOBBES_DONE_"), "{scrubbed}");
        assert!(!scrubbed.contains("__HOBBES_INIT_"), "{scrubbed}");
        assert_eq!(scrubbed, "a\nlog: saw [hobbes-sentinel] here\nb");
        // Nothing to scrub → untouched (source code mentioning the format).
        let src = "format!(\"__HOBBES_DONE_{nonce}__\")\tx";
        assert_eq!(scrub_sentinels(src), src);
    }

    #[test]
    fn finalize_output_never_leaks_prior_command() {
        // The observed leak: a previous command's output and its raw sentinel
        // in front of this command's output.
        let wrapped = "echo mine";
        let sentinel_line = format!("printf '\\n__HOBBES_DONE_{NONCE_B}__ %d\\n' $?");
        let raw = format!(
            "prior-output\r\n\r\n__HOBBES_DONE_{NONCE_A}__ 0\r\n{wrapped}\r\n{sentinel_line}\r\nmine\tcol2\r\n\r\n"
        );
        assert_eq!(finalize_output(&raw, wrapped, &sentinel_line, false), "mine\tcol2");
    }

    #[test]
    fn finalize_output_over_carry_keeps_early_output_and_marks_trim() {
        // TAIL finalizes carry (drained before the timeout) + the rest.
        let wrapped = "slow";
        let sentinel_line = format!("printf '\\n__HOBBES_DONE_{NONCE_A}__ %d\\n' $?");
        let mut acc = b"early\r\n".to_vec();
        acc.extend_from_slice(format!("late\r\n\r\n__HOBBES_DONE_{NONCE_A}__ 0\r\n").as_bytes());
        let (out, code) = split_at_sentinel(&acc, &format!("__HOBBES_DONE_{NONCE_A}__")).unwrap();
        assert_eq!(code, 0);
        assert_eq!(finalize_output(&out, wrapped, &sentinel_line, false), "early\nlate");
        let marked = finalize_output(&out, wrapped, &sentinel_line, true);
        assert!(marked.starts_with("[output truncated"), "{marked}");
        assert!(marked.ends_with("early\nlate"));
    }

    fn test_ctx(id: &str) -> TerminalCtx {
        TerminalCtx {
            session_id: id.into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        }
    }

    #[tokio::test]
    async fn real_shell_output_keeps_tabs() {
        let client = TerminalClient::new();
        let ctx = test_ctx("t5");
        let out = client
            .exec(&ctx, &serde_json::json!({"command": "printf 'A\\tB\\tC\\n'", "timeout_secs": 20}))
            .await
            .unwrap();
        assert!(out.starts_with("A\tB\tC\n"), "got: {out:?}");
        client.shutdown_all().await;
    }

    #[tokio::test]
    async fn timeout_then_tail_then_exec_is_isolated() {
        let client = TerminalClient::new();
        let ctx = test_ctx("t6");
        let out = client
            .exec(
                &ctx,
                &serde_json::json!({"command": "echo first-a; sleep 2; echo first-b", "timeout_secs": 1}),
            )
            .await
            .unwrap();
        assert!(out.contains("still running"), "got: {out}");
        let out = client
            .tail(&ctx, &serde_json::json!({"timeout_secs": 20}))
            .await
            .unwrap();
        assert!(out.contains("first-a") && out.contains("first-b"), "got: {out}");
        assert!(!out.contains("__HOBBES_"), "sentinel leaked: {out}");

        let out = client
            .exec(&ctx, &serde_json::json!({"command": "echo second", "timeout_secs": 20}))
            .await
            .unwrap();
        assert_eq!(out, "second\n(exit code: 0)");
        client.shutdown_all().await;
    }

    #[tokio::test]
    async fn abandoned_exec_output_only_reaches_tail() {
        // The turn is cancelled mid-read: the exec future is dropped. The
        // command keeps running in the shell; its output must not end up in
        // the next EXEC's result.
        let client = TerminalClient::new();
        let ctx = test_ctx("t7");
        // Warm the shell so the cut-off lands in the read, not the spawn.
        client
            .exec(&ctx, &serde_json::json!({"command": "true", "timeout_secs": 20}))
            .await
            .unwrap();
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.exec(
                &ctx,
                &serde_json::json!({"command": "sleep 1; echo orphan-out", "timeout_secs": 30}),
            ),
        )
        .await;
        assert!(abandoned.is_err(), "exec should have been cut off");

        let err = client
            .exec(&ctx, &serde_json::json!({"command": "echo next", "timeout_secs": 20}))
            .await
            .unwrap_err();
        assert!(err.contains("HOBBES_TERMINAL_TAIL"), "got: {err}");

        let out = client
            .tail(&ctx, &serde_json::json!({"timeout_secs": 20}))
            .await
            .unwrap();
        assert!(out.contains("orphan-out"), "got: {out}");
        assert!(!out.contains("__HOBBES_"), "sentinel leaked: {out}");

        let out = client
            .exec(&ctx, &serde_json::json!({"command": "echo next", "timeout_secs": 20}))
            .await
            .unwrap();
        assert_eq!(out, "next\n(exit code: 0)");
        client.shutdown_all().await;
    }

    #[tokio::test]
    async fn concurrent_execs_do_not_cross_contaminate() {
        // Parallel tool calls in one turn each run in their own task.
        let client = TerminalClient::new();
        let ctx = test_ctx("t8");
        let a_args = serde_json::json!({"command": "sleep 0.3; echo out-a", "timeout_secs": 20});
        let b_args = serde_json::json!({"command": "echo out-b", "timeout_secs": 20});
        let (a, b) = tokio::join!(client.exec(&ctx, &a_args), client.exec(&ctx, &b_args));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a, "out-a\n(exit code: 0)");
        assert_eq!(b, "out-b\n(exit code: 0)");
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
    fn bg_ctx(id: &str) -> TerminalCtx {
        TerminalCtx {
            session_id: id.into(),
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            project_id: None,
        }
    }

    /// Run `cmd`, asserting EXEC returns (finished, not "still running")
    /// well inside its timeout.
    async fn exec_finishes(client: &TerminalClient, ctx: &TerminalCtx, cmd: &str) -> String {
        let args = serde_json::json!({"command": cmd, "timeout_secs": 8});
        let out = client.exec(ctx, &args).await.unwrap();
        assert!(!out.contains("still running"), "`{cmd}` hung: {out}");
        out
    }

    #[tokio::test]
    async fn exec_returns_with_background_children() {
        let client = TerminalClient::new();
        let ctx = bg_ctx("bg1");
        for cmd in [
            "sleep 120 &",
            "(sleep 121 &) ; echo done-subshell",
            "nohup sleep 122 </dev/null >/dev/null 2>&1 & disown",
            "sleep 123 & disown; echo disowned",
            // opens the controlling tty like `claude` does
            "(sh -c 'exec 3</dev/tty; sleep 124' </dev/null >/dev/null 2>&1 &) ; echo tty-child",
            "python3 -c \"import os; open('/dev/tty','w').write('x')\" & disown",
        ] {
            exec_finishes(&client, &ctx, cmd).await;
        }
        // The shell is still usable afterwards.
        let out = exec_finishes(&client, &ctx, "echo still-alive").await;
        assert!(out.contains("still-alive"), "{out}");
        client.shutdown_all().await;
    }

    /// A background child that READS the controlling tty (as an interactive
    /// CLI probing for a terminal does). In a subshell (the `cwd` wrapper) it
    /// stays in the foreground process group and would race zsh for the
    /// sentinel line we type next.
    #[tokio::test]
    async fn exec_survives_background_tty_reader() {
        let client = TerminalClient::new();
        let ctx = bg_ctx("bg2");
        let reader = "sh -c 'cat </dev/tty >/dev/null' </dev/null >/dev/null 2>&1";
        exec_finishes(&client, &ctx, &format!("{reader} & disown")).await;
        exec_finishes(&client, &ctx, &format!("({reader} &)")).await;
        let args = serde_json::json!({
            "command": format!("{reader} & disown"),
            "cwd": "/tmp",
            "timeout_secs": 8
        });
        let out = client.exec(&ctx, &args).await.unwrap();
        assert!(!out.contains("still running"), "cwd-wrapped hung: {out}");
        // Give the readers time to start blocking on the tty, then keep going:
        // later commands must not be swallowed by them.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        for i in 0..3 {
            let out = exec_finishes(&client, &ctx, &format!("echo after-reader-{i}")).await;
            assert!(out.contains(&format!("after-reader-{i}")), "{out}");
        }
        client.shutdown_all().await;
    }

    /// Manual repro with the real CLI: `cargo test --release -- --ignored real_claude`.
    #[tokio::test]
    #[ignore]
    async fn real_claude_background_spawn() {
        let client = TerminalClient::new();
        let ctx = bg_ctx("bg-claude");
        let out = exec_finishes(
            &client,
            &ctx,
            "nohup claude -p 'say hi' </dev/null > /tmp/hobbes-bg-claude.log 2>&1 & disown",
        )
        .await;
        eprintln!("first: {out}");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let out = exec_finishes(&client, &ctx, "echo next; stty -a | head -3").await;
        eprintln!("second: {out}");
        client.shutdown_all().await;
    }

    #[tokio::test]
    async fn shell_path_includes_cargo_bin() {
        let Some(home) = dirs::home_dir() else { return };
        let cargo_bin = home.join(".cargo/bin");
        if !cargo_bin.is_dir() {
            return;
        }
        let client = TerminalClient::new();
        let ctx = bg_ctx("bg-path");
        let out = exec_finishes(&client, &ctx, "echo $PATH").await;
        assert!(out.contains(".cargo/bin"), "PATH lacks ~/.cargo/bin: {out}");
        client.shutdown_all().await;
    }

    /// `hobbes_detach` children live in their own session: no controlling
    /// tty, and RESET (which kills the shell and closes the pty) leaves them.
    #[tokio::test]
    async fn detached_spawn_survives_reset_without_tty() {
        let client = TerminalClient::new();
        let ctx = bg_ctx("bg-detach");
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let log = std::env::temp_dir().join(format!("hobbes-detach-{tag}.log"));
        let log_s = log.to_string_lossy().into_owned();
        exec_finishes(
            &client,
            &ctx,
            &format!(
                "hobbes_detach {log_s} sh -c 'if (: </dev/tty) 2>/dev/null; then echo HAS_TTY; else echo NO_TTY; fi; sleep 3; echo ALIVE_{tag}'"
            ),
        )
        .await;
        // Reset immediately: the child must outlive the shell.
        client
            .execute_tool("HOBBES_TERMINAL_RESET", serde_json::json!({}), Some(ctx.clone()))
            .await
            .unwrap();
        let mut text = String::new();
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains(&format!("ALIVE_{tag}")) {
                break;
            }
        }
        let _ = std::fs::remove_file(&log);
        assert!(text.contains("NO_TTY"), "child had a tty: {text:?}");
        assert!(text.contains(&format!("ALIVE_{tag}")), "child killed by RESET: {text:?}");
    }
}
