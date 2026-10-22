//! Click-to-terminal: take the user to the terminal tab a Claude Code
//! session runs in (Fleet redesign).
//!
//! The terminal is identified from environment variables Claude Code's HTTP
//! hooks forward as headers ([`TerminalHint`]). A click walks a fallback
//! ladder and stops at the first rung that works: focus the exact tab
//! (iTerm2 by session id, Terminal.app by tty), bring the hosting app
//! forward, resume an ended session in a new tab, or copy the resume
//! command. [`FocusOutcome::note`] says which rung was reached.

// Scaffold: the implementation pass removes this allow.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use super::FleetSession;

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
        let _ = session_name;
        String::new()
    }
}

/// The terminal app's display name for a session, when known ("iTerm2",
/// "Terminal", "Ghostty", "VS Code"...).
pub fn terminal_name(session: &FleetSession) -> Option<String> {
    let _ = session;
    None
}

/// Label for the row's go-to action: "Focus iTerm2 tab", "Bring VS Code
/// forward", "Copy resume command".
pub fn destination_label(session: &FleetSession) -> String {
    let _ = session;
    String::new()
}

/// Label for a needs-you row's primary action: "Answer in iTerm2".
pub fn answer_label(session: &FleetSession) -> String {
    let _ = session;
    String::new()
}

/// `cd '<cwd>' && claude --resume <id>` (PowerShell form on Windows).
pub fn resume_command(session: &FleetSession) -> Option<String> {
    let _ = session;
    None
}

/// Walk the ladder for this session. Runs blocking work (osascript, ps) off
/// the UI thread; call from a spawned task.
pub async fn go_to_session(session: FleetSession) -> FocusOutcome {
    let _ = session;
    FocusOutcome::Failed("not implemented".to_string())
}
