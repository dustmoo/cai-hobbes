//! Click-to-terminal: take the user to the terminal tab a Claude Code
//! session runs in (Fleet redesign).
//!
//! The terminal is identified from environment variables Claude Code's HTTP
//! hooks forward as headers ([`TerminalHint`], validated at ingest by
//! [`hint_from_headers`]). A click walks a fallback ladder and stops at the
//! first rung that works: focus the exact tab (iTerm2 by session id,
//! Terminal.app by tty), bring the hosting app forward, resume an ended
//! session in a new tab, or copy the resume command. [`FocusOutcome::note`]
//! says which rung was reached.
//!
//! The ladder is chosen by the pure [`plan_ladder`]; the process and
//! AppleScript calls under it are kept thin. Security rules (fleet design
//! §3.6): nothing from a hook payload or header ever reaches a shell. Every
//! process is spawned by absolute path with an argument vector, and the
//! AppleScript sources are fixed strings compiled into the binary that
//! receive their ids through `on run argv`. The only shell text built
//! anywhere is the resume command, from a validated session id and a
//! single-quoted, validated cwd.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{FleetOrigin, FleetSession};

/// iTerm2's bundle id.
pub const ITERM_BUNDLE: &str = "com.googlecode.iterm2";
/// Terminal.app's bundle id.
pub const TERMINAL_BUNDLE: &str = "com.apple.Terminal";
/// Opens System Settings → Privacy & Security → Automation.
pub const AUTOMATION_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation";

/// Longest accepted header value.
const MAX_HINT_BYTES: usize = 256;
/// Budget for each spawned helper (osascript, ps, lsof, open). The ladder
/// falls to the next rung when one runs out.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// A `claude` process whose start time is within this window of the
/// session's last `SessionStart` is a start-time match.
const START_MATCH_SECS: i64 = 120;

/// What the hooks told us about the terminal a session runs in. Every field
/// is validated at ingest; empty strings are stored as `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalHint {
    /// `TERM_PROGRAM` (e.g. "iTerm.app", "Apple_Terminal", "vscode").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_program: Option<String>,
    /// `__CFBundleIdentifier` on macOS: the app hosting the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// `ITERM_SESSION_ID` ("w1t1p0:<UUID>").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterm_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wezterm_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kitty_window: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kitty_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wt_session: Option<String>,
    /// When Hobbes saw the session's last start (`SessionStart` from a
    /// startup or resume). Picks the right `claude` process when several
    /// share a cwd (the Terminal.app tty lookup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
}

/// Where a click ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusOutcome {
    /// The exact tab is in front.
    Focused { app: String },
    /// Only the hosting app could be brought forward.
    Activated { app: String },
    /// An ended session was resumed in a new tab.
    Resumed { app: String },
    /// The resume command is on the clipboard.
    Copied,
    /// Nothing worked; the reason is shown to the user.
    Failed(String),
}

impl FocusOutcome {
    /// One line for the note under the row.
    pub fn note(&self, session_name: &str) -> String {
        match self {
            FocusOutcome::Focused { app } => format!("Switched to {session_name} in {app}."),
            FocusOutcome::Activated { app } => {
                let bundle = bundle_for_app_name(app);
                if bundle.is_some_and(is_denied) {
                    format!(
                        "{app} was brought forward. To jump straight to the tab, allow Hobbes to control {app} in System Settings \u{2192} Privacy & Security \u{2192} Automation."
                    )
                } else if bundle.is_some_and(supports_exact_focus) {
                    format!("Couldn't find the tab, so {app} was brought forward.")
                } else {
                    format!("{} was brought forward.", capitalize(app))
                }
            }
            FocusOutcome::Resumed { app } => format!("Resumed {session_name} in a new {app} window."),
            FocusOutcome::Copied => {
                format!("Resume command for {session_name} copied \u{2014} paste it into a terminal.")
            }
            FocusOutcome::Failed(reason) => reason.clone(),
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

// ── Header capture (ingest) ─────────────────────────────────────────────────

/// One forwarded variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintField {
    TermProgram,
    BundleId,
    Iterm,
    WeztermPane,
    KittyWindow,
    KittySocket,
    Tmux,
    TmuxPane,
    WtSession,
}

/// (header name, environment variable, field). The hook entries forward
/// each variable as `"<header>": "$<VAR>"` with the variable listed in
/// `allowedEnvVars` (`hooks_config`); the listener reads them back here.
pub(crate) const HINT_HEADERS: &[(&str, &str, HintField)] = &[
    ("X-Hobbes-Term", "TERM_PROGRAM", HintField::TermProgram),
    ("X-Hobbes-App", "__CFBundleIdentifier", HintField::BundleId),
    ("X-Hobbes-Iterm", "ITERM_SESSION_ID", HintField::Iterm),
    ("X-Hobbes-Wezterm", "WEZTERM_PANE", HintField::WeztermPane),
    ("X-Hobbes-Kitty", "KITTY_WINDOW_ID", HintField::KittyWindow),
    ("X-Hobbes-Kitty-Sock", "KITTY_LISTEN_ON", HintField::KittySocket),
    ("X-Hobbes-Tmux", "TMUX", HintField::Tmux),
    ("X-Hobbes-Tmux-Pane", "TMUX_PANE", HintField::TmuxPane),
    ("X-Hobbes-Wt", "WT_SESSION", HintField::WtSession),
];

fn all_digits(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
}

/// `8-4-4-4-12` hex.
fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// `^w\d+t\d+p\d+:[0-9A-F-]{36}$` (hex accepted in either case).
fn is_iterm_session(s: &str) -> bool {
    let Some((loc, uuid)) = s.split_once(':') else { return false };
    let Some(rest) = loc.strip_prefix('w') else { return false };
    let Some((w, rest)) = rest.split_once('t') else { return false };
    let Some((t, p)) = rest.split_once('p') else { return false };
    all_digits(w, 9)
        && all_digits(t, 9)
        && all_digits(p, 9)
        && uuid.len() == 36
        && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// An absolute path with no control characters.
fn is_abs_path(s: &str) -> bool {
    s.starts_with('/') && !s.chars().any(char::is_control)
}

/// Validate one header value. `Ok(None)` for an empty value (variable unset
/// or stripped by policy), `Err(())` for anything that fails its pattern —
/// the caller logs the header name only, never the value.
pub(crate) fn validate_hint(field: HintField, raw: &[u8]) -> Result<Option<String>, ()> {
    if raw.len() > MAX_HINT_BYTES {
        return Err(());
    }
    let s = std::str::from_utf8(raw).map_err(|_| ())?.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if s.chars().any(char::is_control) {
        return Err(());
    }
    let ok = match field {
        HintField::TermProgram => {
            s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        }
        HintField::BundleId => {
            s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        }
        HintField::Iterm => is_iterm_session(s),
        HintField::WeztermPane | HintField::KittyWindow => all_digits(s, 9),
        HintField::TmuxPane => s.strip_prefix('%').is_some_and(|n| all_digits(n, 9)),
        HintField::WtSession => is_guid(s),
        // Socket ownership is checked at use, not here.
        HintField::KittySocket => {
            is_abs_path(s) || s.strip_prefix("unix:").is_some_and(|p| is_abs_path(p) || p.starts_with('@'))
        }
        // "<socket path>,<server pid>,<session>".
        HintField::Tmux => is_abs_path(s),
    };
    if ok { Ok(Some(s.to_string())) } else { Err(()) }
}

/// Build a hint from request headers. `get` returns a header's raw bytes.
/// Returns the hint and the names of headers that failed validation.
pub(crate) fn hint_from_headers<'a>(
    get: impl Fn(&str) -> Option<&'a [u8]>,
) -> (TerminalHint, Vec<&'static str>) {
    let mut hint = TerminalHint::default();
    let mut rejected = Vec::new();
    for (header, _, field) in HINT_HEADERS {
        let Some(raw) = get(header) else { continue };
        match validate_hint(*field, raw) {
            Ok(Some(v)) => hint.set(*field, v),
            Ok(None) => {}
            Err(()) => rejected.push(*header),
        }
    }
    (hint, rejected)
}

impl TerminalHint {
    fn slot(&mut self, field: HintField) -> &mut Option<String> {
        match field {
            HintField::TermProgram => &mut self.term_program,
            HintField::BundleId => &mut self.bundle_id,
            HintField::Iterm => &mut self.iterm_session,
            HintField::WeztermPane => &mut self.wezterm_pane,
            HintField::KittyWindow => &mut self.kitty_window,
            HintField::KittySocket => &mut self.kitty_socket,
            HintField::Tmux => &mut self.tmux,
            HintField::TmuxPane => &mut self.tmux_pane,
            HintField::WtSession => &mut self.wt_session,
        }
    }

    fn set(&mut self, field: HintField, value: String) {
        *self.slot(field) = Some(value);
    }

    /// No terminal identity at all (`started_at` alone doesn't count).
    pub fn is_empty(&self) -> bool {
        TerminalHint { started_at: None, ..self.clone() } == TerminalHint::default()
    }

    /// Which app hosts the terminal, as far as identity goes.
    fn host_key(&self) -> Option<&str> {
        self.bundle_id.as_deref().or(self.term_program.as_deref())
    }

    /// Fold a newer hint in. A later non-empty value replaces an older one.
    /// When the newer hint names a different host app (the session was
    /// resumed somewhere else), the older identity is dropped wholesale so a
    /// stale tab id from the old terminal can't linger. Returns whether
    /// anything changed.
    pub(crate) fn merge_from(&mut self, newer: &TerminalHint) -> bool {
        let before = self.clone();
        if let (Some(old), Some(new)) = (self.host_key(), newer.host_key()) {
            if old != new {
                *self = TerminalHint { started_at: self.started_at, ..TerminalHint::default() };
            }
        }
        for (_, _, field) in HINT_HEADERS {
            if let Some(v) = newer.clone().slot(*field).take() {
                self.set(*field, v);
            }
        }
        if newer.started_at.is_some() {
            self.started_at = newer.started_at;
        }
        *self != before
    }
}

// ── Names and labels ────────────────────────────────────────────────────────

/// Bundle id → display name.
const KNOWN_APPS: &[(&str, &str)] = &[
    (ITERM_BUNDLE, "iTerm2"),
    (TERMINAL_BUNDLE, "Terminal"),
    ("com.mitchellh.ghostty", "Ghostty"),
    ("com.github.wez.wezterm", "WezTerm"),
    ("net.kovidgoyal.kitty", "kitty"),
    ("org.alacritty", "Alacritty"),
    ("co.zeit.hyper", "Hyper"),
    ("dev.warp.Warp-Stable", "Warp"),
    ("com.microsoft.VSCode", "VS Code"),
    ("com.microsoft.VSCodeInsiders", "VS Code Insiders"),
    ("com.todesktop.230313mzl4w4u92", "Cursor"),
    ("com.exafunction.windsurf", "Windsurf"),
    ("dev.zed.Zed", "Zed"),
];

/// `TERM_PROGRAM` → (display name, bundle id when unambiguous).
const KNOWN_TERM_PROGRAMS: &[(&str, &str, Option<&str>)] = &[
    ("iTerm.app", "iTerm2", Some(ITERM_BUNDLE)),
    ("Apple_Terminal", "Terminal", Some(TERMINAL_BUNDLE)),
    ("ghostty", "Ghostty", Some("com.mitchellh.ghostty")),
    ("WezTerm", "WezTerm", Some("com.github.wez.wezterm")),
    ("WarpTerminal", "Warp", Some("dev.warp.Warp-Stable")),
    ("Hyper", "Hyper", Some("co.zeit.hyper")),
    // VS Code, Cursor and Windsurf all say "vscode"; the bundle id tells
    // them apart.
    ("vscode", "VS Code", None),
];

fn app_name_for_bundle(bundle: &str) -> Option<&'static str> {
    KNOWN_APPS.iter().find(|(b, _)| *b == bundle).map(|(_, n)| *n)
}

fn bundle_for_app_name(name: &str) -> Option<&'static str> {
    KNOWN_APPS.iter().find(|(_, n)| *n == name).map(|(b, _)| *b)
}

fn term_program_info(tp: &str) -> Option<(&'static str, Option<&'static str>)> {
    KNOWN_TERM_PROGRAMS
        .iter()
        .find(|(p, _, _)| *p == tp)
        .map(|(_, n, b)| (*n, *b))
}

/// The terminal app's display name from a hint, when known. The bundle id
/// wins (it tells Cursor from VS Code); `TERM_PROGRAM` is the fallback.
pub(crate) fn hint_app_name(hint: &TerminalHint) -> Option<&'static str> {
    hint.bundle_id
        .as_deref()
        .and_then(app_name_for_bundle)
        .or_else(|| hint.term_program.as_deref().and_then(term_program_info).map(|(n, _)| n))
        .or_else(|| hint.wt_session.as_ref().map(|_| "Windows Terminal"))
}

/// The bundle id to activate for a hint: the forwarded one, else one implied
/// by `TERM_PROGRAM`.
pub(crate) fn hint_bundle(hint: &TerminalHint) -> Option<String> {
    hint.bundle_id
        .clone()
        .or_else(|| hint.term_program.as_deref().and_then(term_program_info).and_then(|(_, b)| b).map(str::to_string))
}

/// Apps whose tabs can be focused exactly (rung 1).
fn supports_exact_focus(bundle: &str) -> bool {
    bundle == ITERM_BUNDLE || bundle == TERMINAL_BUNDLE
}

/// The terminal app's display name for a session, when known ("iTerm2",
/// "Terminal", "Ghostty", "VS Code"...).
pub fn terminal_name(session: &FleetSession) -> Option<String> {
    session.terminal.as_ref().and_then(hint_app_name).map(str::to_string)
}

/// Label for the row's go-to action: "Focus iTerm2 tab", "Bring VS Code
/// forward", "Copy resume command".
pub fn destination_label(session: &FleetSession) -> String {
    if session.origin == FleetOrigin::Hobbes {
        return "Open its chat".to_string();
    }
    let plan = plan_for(session);
    match plan.first() {
        Some(Rung::FocusIterm) => "Focus iTerm2 tab".to_string(),
        Some(Rung::FocusTerminalApp) => "Focus Terminal tab".to_string(),
        Some(Rung::Activate { app, .. }) => format!("Bring {app} forward"),
        Some(Rung::Resume { app, .. }) => format!("Resume in {app}"),
        Some(Rung::Copy) => "Copy resume command".to_string(),
        None => "No terminal to go to".to_string(),
    }
}

/// Label for a needs-you row's primary action: "Answer in iTerm2".
pub fn answer_label(session: &FleetSession) -> String {
    match terminal_name(session) {
        Some(app) => format!("Answer in {app}"),
        None => "Answer in terminal".to_string(),
    }
}

/// Whether macOS refused Hobbes control of this session's terminal earlier
/// in this run (the UI can link to [`AUTOMATION_SETTINGS_URL`]).
pub fn automation_denied_for(session: &FleetSession) -> bool {
    session
        .terminal
        .as_ref()
        .and_then(hint_bundle)
        .is_some_and(|b| is_denied(&b))
}

// ── Resume command ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shell {
    Posix,
    PowerShell,
}

/// A Claude Code session id: UUID-shaped, nothing a shell would interpret.
pub(crate) fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// An absolute cwd with no NUL, newline or other control characters.
pub(crate) fn valid_cwd(cwd: &str, shell: Shell) -> bool {
    if cwd.is_empty() || cwd.len() > 4096 || cwd.chars().any(char::is_control) {
        return false;
    }
    match shell {
        Shell::Posix => cwd.starts_with('/'),
        Shell::PowerShell => {
            let b = cwd.as_bytes();
            cwd.starts_with("\\\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        }
    }
}

/// Single-quote for the shell: POSIX `'` → `'\''`; PowerShell `'` → `''`.
pub(crate) fn single_quote(s: &str, shell: Shell) -> String {
    let inner = match shell {
        Shell::Posix => s.replace('\'', "'\\''"),
        Shell::PowerShell => s.replace('\'', "''"),
    };
    format!("'{inner}'")
}

pub(crate) fn resume_command_for(cwd: &str, session_id: &str, shell: Shell) -> Option<String> {
    if !valid_session_id(session_id) || !valid_cwd(cwd, shell) {
        return None;
    }
    let dir = single_quote(cwd, shell);
    Some(match shell {
        Shell::Posix => format!("cd {dir} && claude --resume {session_id}"),
        Shell::PowerShell => format!("Set-Location -LiteralPath {dir}; claude --resume {session_id}"),
    })
}

/// `cd '<cwd>' && claude --resume <id>` (PowerShell form on Windows).
pub fn resume_command(session: &FleetSession) -> Option<String> {
    if session.origin == FleetOrigin::Hobbes {
        return None;
    }
    let shell = if cfg!(windows) { Shell::PowerShell } else { Shell::Posix };
    resume_command_for(&session.cwd, &session.id, shell)
}

// ── The ladder (pure) ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Windows,
    Other,
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Other
        }
    }
}

/// One step of the ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rung {
    /// Rung 1: the iTerm2 session by unique id, falling back to tty.
    FocusIterm,
    /// Rung 1: the Terminal.app tab by tty.
    FocusTerminalApp,
    /// Rung 2: bring the hosting app forward (no Automation needed).
    Activate { bundle: String, app: String },
    /// Rung 3: `claude --resume` in a new window of this app.
    Resume { bundle: &'static str, app: &'static str },
    /// Rung 4: the resume command onto the clipboard.
    Copy,
}

/// Choose the rungs to try, in order.
///
/// - Rung 1 only for a live session (an ended row's tab may run something
///   else now) and only when macOS hasn't refused control of that app.
/// - Rung 2 only for a live session: for an ended one, bringing the app
///   forward would "succeed" without getting the user anywhere.
/// - Rung 3 only when the session is NOT live — `claude --resume` on a live
///   session would put a second process on it. It uses the session's own
///   iTerm2/Terminal, or Terminal when the host is unknown (dispatched runs).
/// - Rung 4 whenever a resume command can be built.
pub(crate) fn plan_ladder(
    live: bool,
    hint: Option<&TerminalHint>,
    platform: Platform,
    denied: &dyn Fn(&str) -> bool,
    can_resume: bool,
) -> Vec<Rung> {
    let mut plan = Vec::new();
    if platform == Platform::MacOs {
        let bundle = hint.and_then(hint_bundle);
        if live {
            match bundle.as_deref() {
                Some(ITERM_BUNDLE) if !denied(ITERM_BUNDLE) => plan.push(Rung::FocusIterm),
                Some(TERMINAL_BUNDLE) if !denied(TERMINAL_BUNDLE) => plan.push(Rung::FocusTerminalApp),
                _ => {}
            }
            if let Some(b) = bundle.clone() {
                let app = hint
                    .and_then(hint_app_name)
                    .or_else(|| app_name_for_bundle(&b))
                    .unwrap_or("your terminal")
                    .to_string();
                plan.push(Rung::Activate { bundle: b, app });
            }
        } else if can_resume {
            let host_known = hint.is_some_and(|h| !h.is_empty());
            let target = match bundle.as_deref() {
                Some(ITERM_BUNDLE) => Some((ITERM_BUNDLE, "iTerm2")),
                Some(TERMINAL_BUNDLE) => Some((TERMINAL_BUNDLE, "Terminal")),
                _ if !host_known => Some((TERMINAL_BUNDLE, "Terminal")),
                _ => None,
            };
            if let Some((bundle, app)) = target.filter(|(b, _)| !denied(b)) {
                plan.push(Rung::Resume { bundle, app });
            }
        }
    }
    // Windows: raising the window of the `claude` process's ancestor
    // (WindowsTerminal.exe / conhost) needs Win32 (CreateToolhelp32Snapshot
    // + EnumWindows + SetForegroundWindow) and the `windows` crate is not a
    // dependency yet. TODO(fleet slice 5): add the window raise and the
    // `wt -w 0 nt -d <cwd>` resume rung. Linux has no supported focus API;
    // both stay on the copy rung.
    if can_resume {
        plan.push(Rung::Copy);
    }
    plan
}

fn plan_for(session: &FleetSession) -> Vec<Rung> {
    plan_ladder(
        session.ended_at.is_none(),
        session.terminal.as_ref(),
        Platform::current(),
        &is_denied,
        resume_command(session).is_some(),
    )
}

// ── Automation denials (process lifetime) ───────────────────────────────────

fn denied_set() -> &'static Mutex<HashSet<String>> {
    static DENIED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    DENIED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_denied(bundle: &str) -> bool {
    denied_set().lock().map(|s| s.contains(bundle)).unwrap_or(false)
}

fn remember_denied(bundle: &str) {
    if let Ok(mut s) = denied_set().lock() {
        s.insert(bundle.to_string());
    }
}

/// osascript's stderr when macOS refused (or would need to ask for) the
/// Apple Events permission.
pub(crate) fn is_automation_denial(stderr: &str) -> bool {
    stderr.contains("-1743") || stderr.contains("-1744") || stderr.contains("Not authorized to send Apple events")
}

// ── Process table (option A: resolve the tty at click time) ─────────────────

/// One `claude` process seen in the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcRow {
    pub pid: u32,
    /// `ttys003`, or `??` for none.
    pub tty: String,
    pub started: Option<DateTime<Utc>>,
    pub command: String,
}

/// Parse one `ps -o pid=,tty=,lstart=,command=` line. `lstart` is five
/// tokens in local time ("Wed Oct 21 15:20:40 2026").
pub(crate) fn parse_ps_line(line: &str) -> Option<ProcRow> {
    let mut it = line.split_whitespace();
    let pid = it.next()?.parse().ok()?;
    let tty = it.next()?.to_string();
    let lstart: Vec<&str> = it.by_ref().take(5).collect();
    if lstart.len() != 5 {
        return None;
    }
    let command: Vec<&str> = it.collect();
    if command.is_empty() {
        return None;
    }
    let started = chrono::NaiveDateTime::parse_from_str(&lstart.join(" "), "%a %b %e %H:%M:%S %Y")
        .ok()
        .and_then(|n| n.and_local_timezone(chrono::Local).earliest())
        .map(|d| d.with_timezone(&Utc));
    Some(ProcRow { pid, tty, started, command: command.join(" ") })
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Is this a Claude Code CLI process? The native binary shows as `claude`;
/// an npm install as `node …/claude` or `node …/@anthropic-ai/claude-code/cli.js`.
pub(crate) fn is_claude_command(command: &str) -> bool {
    let mut it = command.split_whitespace();
    let Some(first) = it.next() else { return false };
    if basename(first) == "claude" {
        return true;
    }
    if basename(first).starts_with("node") {
        if let Some(second) = it.next() {
            return basename(second) == "claude" || second.contains("@anthropic-ai/claude-code/cli");
        }
    }
    false
}

/// Pick the process a session runs in, or `None` when it's ambiguous.
/// Candidates must have a tty and a matching cwd. Preference: argv naming
/// the session id (`--resume <id>`, `--session-id <id>`); then the only
/// candidate; then the start time nearest the session's last start (within
/// [`START_MATCH_SECS`], and clearly nearer than the runner-up).
pub(crate) fn choose_process<'a>(
    candidates: &'a [(ProcRow, String)],
    session_id: &str,
    cwds: &[String],
    started_at: Option<DateTime<Utc>>,
) -> Option<&'a ProcRow> {
    let pool: Vec<&ProcRow> = candidates
        .iter()
        .filter(|(p, cwd)| p.tty != "??" && !p.tty.is_empty() && cwds.iter().any(|c| c == cwd))
        .map(|(p, _)| p)
        .collect();
    let named: Vec<&ProcRow> = pool
        .iter()
        .copied()
        .filter(|p| p.command.split_whitespace().any(|t| t == session_id))
        .collect();
    if named.len() == 1 {
        return Some(named[0]);
    }
    if pool.len() == 1 {
        return Some(pool[0]);
    }
    let at = started_at?;
    let mut by_gap: Vec<(i64, &ProcRow)> = pool
        .iter()
        .filter_map(|p| p.started.map(|s| ((s - at).num_seconds().abs(), *p)))
        .filter(|(gap, _)| *gap <= START_MATCH_SECS)
        .collect();
    by_gap.sort_by_key(|(gap, _)| *gap);
    match by_gap.as_slice() {
        [(_, only)] => Some(*only),
        [(g0, best), (g1, _), ..] if g1 - g0 > 5 => Some(*best),
        _ => None,
    }
}

/// `lsof -Fn` output → pid → cwd.
pub(crate) fn parse_lsof_cwds(out: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let mut pid = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
            map.insert(p, n.to_string());
        }
    }
    map
}

/// The iTerm2 AppleScript `unique id` is the UUID after the colon of
/// `ITERM_SESSION_ID`.
pub(crate) fn iterm_unique_id(iterm_session: &str) -> Option<&str> {
    let (_, uuid) = iterm_session.split_once(':')?;
    (uuid.len() == 36).then_some(uuid)
}

// ── Process plumbing ────────────────────────────────────────────────────────

struct CmdOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Spawn `bin` (absolute path) with `args`, never through a shell, and wait
/// up to [`COMMAND_TIMEOUT`]. Pipes drain on their own threads so a large
/// output (the process table) can't stall the child.
fn run(bin: &str, args: &[&str]) -> Result<CmdOut, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out_t = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_t = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{bin} timed out"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => return Err(format!("{bin}: {e}")),
        }
    };
    Ok(CmdOut {
        ok: status.success(),
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
    })
}

#[derive(Debug)]
enum ScriptError {
    /// macOS refused Apple Events to the target app.
    Denied,
    Other(String),
}

/// Run a fixed AppleScript with `args` delivered through `on run argv`.
/// Returns the script's trimmed result.
fn osascript(script: &str, args: &[&str]) -> Result<String, ScriptError> {
    let mut argv = vec!["-e", script];
    argv.extend_from_slice(args);
    let out = run("/usr/bin/osascript", &argv).map_err(ScriptError::Other)?;
    if out.ok {
        Ok(out.stdout.trim().to_string())
    } else if is_automation_denial(&out.stderr) {
        Err(ScriptError::Denied)
    } else {
        Err(ScriptError::Other(crate::log_utils::truncate_for_log(out.stderr.trim(), 200)))
    }
}

/// Is an app with this bundle id running? LaunchServices only — no Apple
/// Events, and never launches anything.
fn app_running(bundle: &str) -> bool {
    run("/usr/bin/lsappinfo", &["find", &format!("bundleid={bundle}")])
        .is_ok_and(|o| o.stdout.contains("ASN:"))
}

/// The tty (`/dev/ttys003`) of the `claude` process running this session.
fn resolve_tty(session: &FleetSession) -> Option<String> {
    let ps = run("/bin/ps", &["-axww", "-o", "pid=,tty=,lstart=,command="]).ok()?;
    let rows: Vec<ProcRow> = ps
        .stdout
        .lines()
        .filter_map(parse_ps_line)
        .filter(|p| is_claude_command(&p.command) && p.tty != "??")
        .collect();
    if rows.is_empty() {
        return None;
    }
    let pids = rows.iter().map(|p| p.pid.to_string()).collect::<Vec<_>>().join(",");
    let lsof = run("/usr/sbin/lsof", &["-w", "-a", "-d", "cwd", "-p", &pids, "-Fn"]).ok()?;
    let cwds = parse_lsof_cwds(&lsof.stdout);
    let candidates: Vec<(ProcRow, String)> = rows
        .into_iter()
        .filter_map(|p| cwds.get(&p.pid).cloned().map(|c| (p, c)))
        .collect();
    let mut wanted = vec![session.cwd.trim_end_matches('/').to_string()];
    if let Ok(real) = std::fs::canonicalize(&session.cwd) {
        wanted.push(real.to_string_lossy().into_owned());
    }
    let started_at = session.terminal.as_ref().and_then(|h| h.started_at);
    choose_process(&candidates, &session.id, &wanted, started_at).map(|p| format!("/dev/{}", p.tty))
}

const ITERM_FOCUS_SCRIPT: &str = r#"on run argv
	set targetId to item 1 of argv
	set targetTty to item 2 of argv
	if application id "com.googlecode.iterm2" is not running then return "notrunning"
	tell application id "com.googlecode.iterm2"
		repeat with w in windows
			repeat with t in tabs of w
				repeat with s in sessions of t
					set hit to false
					if targetId is not "" then
						if (unique id of s) is targetId then set hit to true
					end if
					if (not hit) and targetTty is not "" then
						if (tty of s) is targetTty then set hit to true
					end if
					if hit then
						select w
						select t
						select s
						activate
						return "focused"
					end if
				end repeat
			end repeat
		end repeat
	end tell
	return "notfound"
end run"#;

const TERMINAL_FOCUS_SCRIPT: &str = r#"on run argv
	set targetTty to item 1 of argv
	if application id "com.apple.Terminal" is not running then return "notrunning"
	tell application id "com.apple.Terminal"
		repeat with w in windows
			repeat with t in tabs of w
				if (tty of t) is targetTty then
					set selected of t to true
					set index of w to 1
					activate
					return "focused"
				end if
			end repeat
		end repeat
	end tell
	return "notfound"
end run"#;

const ITERM_RESUME_SCRIPT: &str = r#"on run argv
	tell application id "com.googlecode.iterm2"
		activate
		set w to (create window with default profile)
		tell current session of w to write text (item 1 of argv)
	end tell
	return "resumed"
end run"#;

const TERMINAL_RESUME_SCRIPT: &str = r#"on run argv
	tell application id "com.apple.Terminal"
		activate
		do script (item 1 of argv)
	end tell
	return "resumed"
end run"#;

/// What one rung attempt came to.
enum Attempt {
    Done(FocusOutcome),
    Next,
}

fn script_attempt(bundle: &str, result: Result<String, ScriptError>, done: FocusOutcome) -> Attempt {
    match result {
        Ok(r) if r == "focused" || r == "resumed" => Attempt::Done(done),
        Ok(_) => Attempt::Next,
        Err(ScriptError::Denied) => {
            tracing::info!("fleet: Automation permission denied for {bundle}; using app activation");
            remember_denied(bundle);
            Attempt::Next
        }
        Err(ScriptError::Other(e)) => {
            tracing::debug!("fleet: terminal script for {bundle} failed: {e}");
            Attempt::Next
        }
    }
}

fn try_rung(rung: &Rung, session: &FleetSession, resume: Option<&str>) -> Attempt {
    match rung {
        Rung::FocusIterm => {
            if !app_running(ITERM_BUNDLE) {
                return Attempt::Next;
            }
            let done = FocusOutcome::Focused { app: "iTerm2".into() };
            let uid = session
                .terminal
                .as_ref()
                .and_then(|h| h.iterm_session.as_deref())
                .and_then(iterm_unique_id);
            if let Some(uid) = uid {
                match script_attempt(ITERM_BUNDLE, osascript(ITERM_FOCUS_SCRIPT, &[uid, ""]), done.clone()) {
                    Attempt::Done(o) => return Attempt::Done(o),
                    Attempt::Next if is_denied(ITERM_BUNDLE) => return Attempt::Next,
                    Attempt::Next => {}
                }
            }
            match resolve_tty(session) {
                Some(tty) => script_attempt(ITERM_BUNDLE, osascript(ITERM_FOCUS_SCRIPT, &["", &tty]), done),
                None => Attempt::Next,
            }
        }
        Rung::FocusTerminalApp => {
            if !app_running(TERMINAL_BUNDLE) {
                return Attempt::Next;
            }
            match resolve_tty(session) {
                Some(tty) => script_attempt(
                    TERMINAL_BUNDLE,
                    osascript(TERMINAL_FOCUS_SCRIPT, &[&tty]),
                    FocusOutcome::Focused { app: "Terminal".into() },
                ),
                None => Attempt::Next,
            }
        }
        Rung::Activate { bundle, app } => {
            // A live row whose terminal app isn't running is a session that
            // died without a SessionEnd: don't launch the app for it.
            if !app_running(bundle) {
                return Attempt::Next;
            }
            match run("/usr/bin/open", &["-b", bundle]) {
                Ok(o) if o.ok => Attempt::Done(FocusOutcome::Activated { app: app.clone() }),
                _ => Attempt::Next,
            }
        }
        Rung::Resume { bundle, app } => {
            let Some(cmd) = resume else { return Attempt::Next };
            let script = if *bundle == ITERM_BUNDLE { ITERM_RESUME_SCRIPT } else { TERMINAL_RESUME_SCRIPT };
            script_attempt(bundle, osascript(script, &[cmd]), FocusOutcome::Resumed { app: (*app).into() })
        }
        Rung::Copy => match resume {
            Some(cmd) => match feature_clipboard::copy_to_clipboard(cmd) {
                Ok(()) => Attempt::Done(FocusOutcome::Copied),
                Err(_) => Attempt::Done(FocusOutcome::Failed("Couldn't copy the resume command.".into())),
            },
            None => Attempt::Next,
        },
    }
}

fn walk_ladder(session: &FleetSession) -> FocusOutcome {
    let resume = resume_command(session);
    for rung in plan_for(session) {
        if let Attempt::Done(outcome) = try_rung(&rung, session, resume.as_deref()) {
            return outcome;
        }
    }
    FocusOutcome::Failed("Couldn't find a terminal for this session.".to_string())
}

/// Walk the ladder for this session. Runs blocking work (osascript, ps) off
/// the UI thread; call from a spawned task.
pub async fn go_to_session(session: FleetSession) -> FocusOutcome {
    if session.origin == FleetOrigin::Hobbes {
        return FocusOutcome::Failed("This is a Hobbes chat \u{2014} open its tab instead.".to_string());
    }
    match tokio::task::spawn_blocking(move || walk_ladder(&session)).await {
        Ok(outcome) => outcome,
        Err(e) => FocusOutcome::Failed(format!("Couldn't reach the terminal ({e}).")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "68F0CAC3-3C5D-482C-AC4A-BE7854943045";

    fn hint(bundle: Option<&str>, tp: Option<&str>) -> TerminalHint {
        TerminalHint {
            bundle_id: bundle.map(str::to_string),
            term_program: tp.map(str::to_string),
            ..TerminalHint::default()
        }
    }

    fn never(_: &str) -> bool {
        false
    }

    #[test]
    fn header_values_validate_strictly() {
        use HintField::*;
        let ok = |f, v: &str| validate_hint(f, v.as_bytes());
        assert_eq!(ok(Iterm, &format!("w1t1p0:{UUID}")), Ok(Some(format!("w1t1p0:{UUID}"))));
        assert_eq!(ok(Iterm, UUID), Err(()), "needs the w/t/p prefix");
        assert_eq!(ok(Iterm, &format!("w1t1p0:{UUID}; rm -rf /")), Err(()));
        assert_eq!(ok(BundleId, "com.googlecode.iterm2"), Ok(Some("com.googlecode.iterm2".into())));
        assert_eq!(ok(BundleId, "com.evil app"), Err(()));
        assert_eq!(ok(TermProgram, "iTerm.app"), Ok(Some("iTerm.app".into())));
        assert_eq!(ok(TermProgram, "Apple_Terminal"), Ok(Some("Apple_Terminal".into())));
        assert_eq!(ok(TermProgram, "$(whoami)"), Err(()));
        assert_eq!(ok(WeztermPane, "12"), Ok(Some("12".into())));
        assert_eq!(ok(KittyWindow, "1234567890"), Err(()), "ten digits");
        assert_eq!(ok(TmuxPane, "%3"), Ok(Some("%3".into())));
        assert_eq!(ok(TmuxPane, "3"), Err(()));
        assert_eq!(ok(Tmux, "/private/tmp/tmux-501/default,123,0"), Ok(Some("/private/tmp/tmux-501/default,123,0".into())));
        assert_eq!(ok(Tmux, "relative,1,0"), Err(()));
        assert_eq!(ok(KittySocket, "unix:/tmp/kitty-1"), Ok(Some("unix:/tmp/kitty-1".into())));
        assert_eq!(ok(KittySocket, "unix:@kitty"), Ok(Some("unix:@kitty".into())));
        assert_eq!(ok(KittySocket, "tcp:localhost:1"), Err(()));
        assert_eq!(ok(WtSession, "0f0b1c2d-3e4f-5a6b-7c8d-9e0f1a2b3c4d"), Ok(Some("0f0b1c2d-3e4f-5a6b-7c8d-9e0f1a2b3c4d".into())));
        assert_eq!(ok(WtSession, "not-a-guid"), Err(()));
        // Empty (unset or stripped by policy) is "no value", not a failure.
        assert_eq!(ok(Iterm, ""), Ok(None));
        assert_eq!(ok(TermProgram, "   "), Ok(None));
        // Control characters, oversized values and non-UTF-8 fail.
        assert_eq!(ok(Tmux, "/tmp/a\nb"), Err(()));
        assert_eq!(ok(Tmux, &format!("/{}", "a".repeat(300))), Err(()));
        assert_eq!(validate_hint(BundleId, &[0xff, 0xfe]), Err(()));
    }

    #[test]
    fn hint_from_headers_collects_valid_values_and_names_rejects() {
        let iterm = format!("w2t0p1:{UUID}");
        let headers: Vec<(&str, &[u8])> = vec![
            ("X-Hobbes-Term", b"iTerm.app"),
            ("X-Hobbes-App", b"com.googlecode.iterm2"),
            ("X-Hobbes-Iterm", iterm.as_bytes()),
            ("X-Hobbes-Tmux-Pane", b"bogus"),
            ("X-Hobbes-Wt", b""),
        ];
        let (h, rejected) =
            hint_from_headers(|name| headers.iter().find(|(n, _)| *n == name).map(|(_, v)| *v));
        assert_eq!(h.term_program.as_deref(), Some("iTerm.app"));
        assert_eq!(h.bundle_id.as_deref(), Some(ITERM_BUNDLE));
        assert_eq!(h.iterm_session.as_deref(), Some(iterm.as_str()));
        assert_eq!(h.tmux_pane, None);
        assert_eq!(h.wt_session, None);
        assert_eq!(rejected, vec!["X-Hobbes-Tmux-Pane"]);
    }

    #[test]
    fn merge_replaces_with_newer_values_and_drops_a_stale_host() {
        let mut h = hint(Some(ITERM_BUNDLE), Some("iTerm.app"));
        h.iterm_session = Some(format!("w1t1p0:{UUID}"));
        // Same host, newer tab id → replaced; absent values keep the old.
        let mut newer = hint(Some(ITERM_BUNDLE), None);
        newer.iterm_session = Some(format!("w1t2p0:{UUID}"));
        assert!(h.merge_from(&newer));
        assert_eq!(h.iterm_session.as_deref(), Some(format!("w1t2p0:{UUID}").as_str()));
        assert_eq!(h.term_program.as_deref(), Some("iTerm.app"));
        // Nothing new → unchanged.
        assert!(!h.merge_from(&TerminalHint::default()));
        // Resumed in Terminal.app: the iTerm id must not linger.
        assert!(h.merge_from(&hint(Some(TERMINAL_BUNDLE), Some("Apple_Terminal"))));
        assert_eq!(h.iterm_session, None);
        assert_eq!(h.bundle_id.as_deref(), Some(TERMINAL_BUNDLE));
        assert!(!h.is_empty());
        assert!(TerminalHint { started_at: Some(Utc::now()), ..Default::default() }.is_empty());
    }

    #[test]
    fn iterm_unique_id_is_the_uuid_after_the_colon() {
        assert_eq!(iterm_unique_id(&format!("w1t1p0:{UUID}")), Some(UUID));
        assert_eq!(iterm_unique_id(UUID), None);
        assert_eq!(iterm_unique_id("w1t1p0:short"), None);
    }

    #[test]
    fn resume_command_quotes_for_both_shells() {
        let id = "8a4d2c1e-0000-4000-8000-000000000001";
        assert_eq!(
            resume_command_for("/Users/x/dev/hobbes", id, Shell::Posix).unwrap(),
            format!("cd '/Users/x/dev/hobbes' && claude --resume {id}")
        );
        assert_eq!(
            resume_command_for("/Users/x/Bob's $HOME `x`", id, Shell::Posix).unwrap(),
            format!("cd '/Users/x/Bob'\\''s $HOME `x`' && claude --resume {id}")
        );
        assert_eq!(
            resume_command_for(r"C:\Users\Bob's dev", id, Shell::PowerShell).unwrap(),
            format!(r"Set-Location -LiteralPath 'C:\Users\Bob''s dev'; claude --resume {id}")
        );
        // Unsafe parts → no command at all.
        assert_eq!(resume_command_for("relative/dir", id, Shell::Posix), None);
        assert_eq!(resume_command_for("/tmp/a\nrm -rf ~", id, Shell::Posix), None);
        assert_eq!(resume_command_for("/tmp", "id; rm -rf ~", Shell::Posix), None);
        assert_eq!(resume_command_for("/tmp", "", Shell::Posix), None);
        assert_eq!(resume_command_for("/tmp", id, Shell::PowerShell), None, "not a Windows path");
    }

    #[test]
    fn names_come_from_the_bundle_id_then_term_program() {
        assert_eq!(hint_app_name(&hint(Some(ITERM_BUNDLE), Some("tmux"))), Some("iTerm2"));
        assert_eq!(hint_app_name(&hint(None, Some("Apple_Terminal"))), Some("Terminal"));
        // Cursor reports TERM_PROGRAM=vscode; the bundle id disambiguates.
        assert_eq!(hint_app_name(&hint(Some("com.todesktop.230313mzl4w4u92"), Some("vscode"))), Some("Cursor"));
        assert_eq!(hint_app_name(&hint(None, Some("vscode"))), Some("VS Code"));
        assert_eq!(hint_app_name(&hint(Some("com.example.unknown"), None)), None);
        let wt = TerminalHint { wt_session: Some("x".into()), ..Default::default() };
        assert_eq!(hint_app_name(&wt), Some("Windows Terminal"));
        assert_eq!(hint_bundle(&hint(None, Some("iTerm.app"))).as_deref(), Some(ITERM_BUNDLE));
        assert_eq!(hint_bundle(&hint(None, Some("vscode"))), None);
    }

    #[test]
    fn ladder_for_live_and_ended_sessions() {
        let mac = Platform::MacOs;
        let iterm = hint(Some(ITERM_BUNDLE), Some("iTerm.app"));
        let term = hint(Some(TERMINAL_BUNDLE), Some("Apple_Terminal"));
        let vscode = hint(Some("com.microsoft.VSCode"), Some("vscode"));
        let activate = |b: &str, app: &str| Rung::Activate { bundle: b.into(), app: app.into() };

        // Live: exact focus → activate → copy.
        assert_eq!(
            plan_ladder(true, Some(&iterm), mac, &never, true),
            vec![Rung::FocusIterm, activate(ITERM_BUNDLE, "iTerm2"), Rung::Copy]
        );
        assert_eq!(
            plan_ladder(true, Some(&term), mac, &never, true),
            vec![Rung::FocusTerminalApp, activate(TERMINAL_BUNDLE, "Terminal"), Rung::Copy]
        );
        assert_eq!(
            plan_ladder(true, Some(&vscode), mac, &never, true),
            vec![activate("com.microsoft.VSCode", "VS Code"), Rung::Copy]
        );
        // Automation denied for iTerm2: straight to activation.
        let denied = |b: &str| b == ITERM_BUNDLE;
        assert_eq!(
            plan_ladder(true, Some(&iterm), mac, &denied, true),
            vec![activate(ITERM_BUNDLE, "iTerm2"), Rung::Copy]
        );
        // Live sessions never resume (two processes on one session).
        assert!(!plan_ladder(true, Some(&iterm), mac, &never, true)
            .iter()
            .any(|r| matches!(r, Rung::Resume { .. })));
        // No hint while live: only the copy.
        assert_eq!(plan_ladder(true, None, mac, &never, true), vec![Rung::Copy]);

        // Ended: resume in its own terminal, then copy; never focus/activate.
        assert_eq!(
            plan_ladder(false, Some(&iterm), mac, &never, true),
            vec![Rung::Resume { bundle: ITERM_BUNDLE, app: "iTerm2" }, Rung::Copy]
        );
        // Unknown host (a dispatched run): Terminal.
        assert_eq!(
            plan_ladder(false, None, mac, &never, true),
            vec![Rung::Resume { bundle: TERMINAL_BUNDLE, app: "Terminal" }, Rung::Copy]
        );
        // A known host without a resume adapter: copy only.
        assert_eq!(plan_ladder(false, Some(&vscode), mac, &never, true), vec![Rung::Copy]);
        // Denied resume target: copy.
        assert_eq!(plan_ladder(false, Some(&iterm), mac, &denied, true), vec![Rung::Copy]);
        // No valid resume command: nothing that needs one.
        assert!(plan_ladder(false, Some(&iterm), mac, &never, false).is_empty());

        // Windows and Linux: the copy rung.
        let wt = TerminalHint { wt_session: Some("x".into()), ..Default::default() };
        assert_eq!(plan_ladder(true, Some(&wt), Platform::Windows, &never, true), vec![Rung::Copy]);
        assert_eq!(plan_ladder(true, Some(&iterm), Platform::Other, &never, true), vec![Rung::Copy]);
    }

    fn session_with(hint: Option<TerminalHint>, ended: bool) -> FleetSession {
        let mut s: FleetSession = serde_json::from_str(
            r#"{"id":"8a4d2c1e-0000-4000-8000-000000000001","cwd":"/Users/x/dev/hobbes","name":"hobbes",
                "status":"Idle","last_event_at":"2026-10-20T10:00:00Z","working_since":null}"#,
        )
        .unwrap();
        s.terminal = hint;
        if ended {
            s.ended_at = Some(Utc::now());
        }
        s
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn labels_follow_the_first_rung() {
        let iterm = hint(Some(ITERM_BUNDLE), Some("iTerm.app"));
        let live = session_with(Some(iterm.clone()), false);
        assert_eq!(terminal_name(&live).as_deref(), Some("iTerm2"));
        assert_eq!(destination_label(&live), "Focus iTerm2 tab");
        assert_eq!(answer_label(&live), "Answer in iTerm2");
        assert_eq!(destination_label(&session_with(Some(iterm), true)), "Resume in iTerm2");
        let vs = session_with(Some(hint(Some("com.microsoft.VSCode"), Some("vscode"))), false);
        assert_eq!(destination_label(&vs), "Bring VS Code forward");
        assert_eq!(destination_label(&session_with(None, false)), "Copy resume command");
        assert_eq!(answer_label(&session_with(None, false)), "Answer in terminal");
        let mut chat = session_with(None, false);
        chat.origin = FleetOrigin::Hobbes;
        assert_eq!(resume_command(&chat), None);
    }

    #[test]
    fn resume_command_uses_the_session_cwd_and_id() {
        let s = session_with(None, true);
        if cfg!(windows) {
            assert_eq!(resume_command(&s), None, "a POSIX cwd isn't a Windows path");
        } else {
            assert_eq!(
                resume_command(&s).unwrap(),
                "cd '/Users/x/dev/hobbes' && claude --resume 8a4d2c1e-0000-4000-8000-000000000001"
            );
        }
    }

    #[test]
    fn notes_name_the_rung() {
        assert_eq!(
            FocusOutcome::Focused { app: "iTerm2".into() }.note("hobbes"),
            "Switched to hobbes in iTerm2."
        );
        assert_eq!(
            FocusOutcome::Activated { app: "VS Code".into() }.note("hobbes"),
            "VS Code was brought forward."
        );
        assert_eq!(
            FocusOutcome::Activated { app: "Terminal".into() }.note("hobbes"),
            "Couldn't find the tab, so Terminal was brought forward."
        );
        assert!(FocusOutcome::Copied.note("hobbes").contains("copied"));
        assert_eq!(FocusOutcome::Failed("nope".into()).note("x"), "nope");
        // A remembered denial explains how to allow it.
        remember_denied("com.mitchellh.ghostty");
        assert!(FocusOutcome::Activated { app: "Ghostty".into() }
            .note("x")
            .contains("Privacy & Security \u{2192} Automation"));
    }

    #[test]
    fn automation_denial_is_recognised_from_stderr() {
        assert!(is_automation_denial(
            "execution error: Not authorized to send Apple events to iTerm2. (-1743)"
        ));
        assert!(!is_automation_denial("execution error: Can't get window 1. (-1728)"));
    }

    #[test]
    fn ps_lines_parse_and_claude_processes_are_recognised() {
        let row = parse_ps_line("73768 ttys000  Wed Oct 21 15:20:40 2026     claude --continue").unwrap();
        assert_eq!(row.pid, 73768);
        assert_eq!(row.tty, "ttys000");
        assert_eq!(row.command, "claude --continue");
        assert!(row.started.is_some());
        let single_digit_day = parse_ps_line("12 ttys001 Mon Oct  5 09:01:02 2026 claude").unwrap();
        assert!(single_digit_day.started.is_some());
        assert!(parse_ps_line("garbage").is_none());

        assert!(is_claude_command("claude --resume abc"));
        assert!(is_claude_command("/opt/homebrew/bin/claude"));
        assert!(is_claude_command("node /usr/local/bin/claude --continue"));
        assert!(is_claude_command("node /x/node_modules/@anthropic-ai/claude-code/cli.js"));
        assert!(!is_claude_command("vim claude.md"));
        assert!(!is_claude_command("/Applications/Claude.app/Contents/MacOS/Claude"));
    }

    #[test]
    fn lsof_output_maps_pids_to_cwds() {
        let m = parse_lsof_cwds("p1803\nfcwd\nn/Users/x/dev/a\np1836\nfcwd\nn/Users/x/dev/b\n");
        assert_eq!(m.get(&1803).map(String::as_str), Some("/Users/x/dev/a"));
        assert_eq!(m.get(&1836).map(String::as_str), Some("/Users/x/dev/b"));
    }

    #[test]
    fn process_choice_prefers_argv_then_uniqueness_then_start_time() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let p = |pid, tty: &str, started: &str, cmd: &str, cwd: &str| {
            (
                ProcRow { pid, tty: tty.into(), started: Some(t(started)), command: cmd.into() },
                cwd.to_string(),
            )
        };
        let cwd = vec!["/w/a".to_string()];
        let rows = vec![
            p(1, "ttys001", "2026-10-22T09:00:00Z", "claude --continue", "/w/a"),
            p(2, "ttys002", "2026-10-22T10:00:00Z", "claude --resume sid-1", "/w/a"),
            p(3, "ttys003", "2026-10-22T10:00:00Z", "claude", "/w/b"),
            p(4, "??", "2026-10-22T10:00:00Z", "claude -p x --session-id sid-9", "/w/a"),
        ];
        // argv names the session.
        assert_eq!(choose_process(&rows, "sid-1", &cwd, None).unwrap().pid, 2);
        // Two candidates in /w/a, no argv match, no start time: ambiguous.
        assert!(choose_process(&rows, "sid-x", &cwd, None).is_none());
        // Start time picks the nearer one.
        assert_eq!(choose_process(&rows, "sid-x", &cwd, Some(t("2026-10-22T09:00:03Z"))).unwrap().pid, 1);
        // Too far from either start: ambiguous.
        assert!(choose_process(&rows, "sid-x", &cwd, Some(t("2026-10-22T12:00:00Z"))).is_none());
        // A unique cwd match needs nothing else; tty-less processes never match.
        assert_eq!(choose_process(&rows, "sid-x", &["/w/b".to_string()], None).unwrap().pid, 3);
        assert!(choose_process(&rows, "sid-9", &["/w/c".to_string()], None).is_none());
    }
}
