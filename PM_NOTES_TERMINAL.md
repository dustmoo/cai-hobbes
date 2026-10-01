# Terminal background-process hang — triage notes

## How the terminal works (src/mcp/terminal_client.rs)
- One `/bin/zsh -f` per chat session on a portable_pty pty. A reader thread feeds a capped ring.
- **Completion detection is already a per-command sentinel**: EXEC writes `<cmd>\n printf '\n__HOBBES_DONE_<nonce>__ %d\n' $?`
  and waits for that line. It does NOT wait for pty EOF or the process group. A timeout returns "still running"; TAIL resumes.
- **RESET** = drop the session → `child.kill()` on the zsh pid, then the pty master closes (kernel SIGHUPs the session's pgrp;
  orphaned stopped pgrps get HUP+CONT). No killpg.
- **Env/PATH**: `zsh -f` skips .zshrc; init sources /etc/zprofile and ~/.zprofile only. No explicit PATH repair existed.

## Root cause — what I could and could not establish
- I could NOT reproduce a hang. Tests for `sleep &`, `(sleep &)`, `nohup … & disown`, children opening /dev/tty, children
  reading /dev/tty (incl. through the `cwd` subshell wrapper) and the real `nohup claude -p … </dev/null >log 2>&1 & disown`
  all return immediately with the sentinel. So the "exec waits for EOF/pgrp" hypothesis is wrong for this code.
  (Interactive zsh puts each job in its own pgrp, so a bg tty reader gets EIO/SIGTTIN rather than stealing our input.)
- Confirmed gaps: (1) login PATH lacked ~/.cargo/bin, ~/.local/bin, Homebrew; (2) nothing let an agent start a process that
  survives RESET / shell death, so detached children were HUPed or lost with the session.
- Likeliest remaining explanations for the observed hang (unverified): the command text itself left zsh waiting
  (unbalanced quote/heredoc in the prompt string swallowing the sentinel line), or a child changing tty modes. Please capture
  the exact command string from the stuck call if it recurs.

## Fix
- `PATH_FIXUP` appends ~/.cargo/bin, ~/.local/bin, /opt/homebrew/{bin,sbin}, /usr/local/bin when present and missing.
- `hobbes_detach LOGFILE cmd args…` shell function defined at shell init: perl double-fork + `setsid`, stdin /dev/null,
  output appended to LOGFILE → own session, no controlling tty, survives RESET. Documented in the EXEC tool description.
- RESET semantics unchanged (still kills the shell); only non-detached children are affected.

## Tests (all in terminal_client.rs)
`exec_returns_with_background_children`, `exec_survives_background_tty_reader`, `shell_path_includes_cargo_bin`,
`detached_spawn_survives_reset_without_tty`, plus ignored manual `real_claude_background_spawn`.
Full `cargo test --release` (1195 pass) and `cargo clippy --release` clean.

## Remaining risks
- Hang not reproduced; the fix does not claim to cure it. No watchdog exists if a sentinel line is swallowed by shell parse state.
- `nohup … & disown` children can still be HUPed on RESET if they reset SIGHUP handling; tell agents to use `hobbes_detach`.
- hobbes_detach needs perl (present on macOS); Windows has no terminal path here.
- SDKROOT (CLT link failure) is a build-env issue, not set by the terminal.
