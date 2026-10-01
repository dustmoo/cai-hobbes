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

---

# Follow-up: replay of the exact hung strings (2026-10-01)

## Result: reproduced, two independent root causes
Both strings in `REPRO_COMMANDS.md` (stored byte-for-byte in `src/mcp/repro/hang{1,2}.txt`, replayed as ONE EXEC with a
fake `claude` on PATH that opens /dev/tty, `stty raw -echo`s it or reads it) hung the old code. Bisecting down to bare
zsh-on-a-pty one-liners:

1. **`"pid $!"` — BANG_HIST (hang 1).** `zsh -f` is interactive, so `!` history expansion is on; `!"` (a bang right before
   a closing double quote) makes zsh wait for more input. PS2 was set to `''`, so the shell sat silently in a `dquote>`
   state and swallowed the sentinel line typed after the command. `echo "pid $!"` alone hangs; `echo "pid $! x"` doesn't.
2. **Any single line over ~1024 bytes (hang 2).** The pty is in canonical mode; the line discipline drops input past
   MAX_CANON (1024) with a BEL per dropped byte, and the newline is dropped with it, so the shell never sees the line end.
   Hang 2's `cd $P && nohup claude -p "…"` line is 1144 bytes. That also explains why nothing at all came back, not even
   the leading `git diff --stat`: the whole typed block stalled. (Where "the second claude ran but the first didn't" came
   from: the 725-byte keepalive line fit; the 1144-byte one did not.)

Not causes (all returned in ~0.1s with the sentinel): multi-line body with `cd … &&` chains, two back-to-back background
launches, children opening/reading /dev/tty, children flipping the tty raw before/after their redirects. A background job
that does `stty` does get `suspended (tty output)` in the capture, but that doesn't block the sentinel.
Older code (27ee5e0, 1f92ccd, d7e1eab) types commands into the tty identically, so the hang goes back to the original
design; the diff d7e1eab..f702b9d on terminal_client.rs is additions only (PATH fixup, hobbes_detach, tests).
The real `claude` binary, replayed once with the same shape (>1 KB prompt line, `echo "pid $!"`, two back-to-back
`nohup … & disown` launches, benign prompt so no agent touches another worktree): `replay_real_claude_shapes` (#[ignore]) —
returns immediately with the fix.

## Fix (not just diagnostics)
- EXEC no longer types the command: it writes it to a 0600 temp file (first line `command rm -f --` itself) and types
  `. '/tmp/hobbes-cmd-<nonce>.zsh'`, then the sentinel line. Length limits, `!` expansion and PS2 continuation no longer
  apply; an unbalanced quote is now a parse error ("unmatched \"") with a non-zero exit instead of a hang. `cd`/exports/
  functions still persist (it is `source`, not a subshell). Falls back to typing if the file can't be written.
- Init also runs `unsetopt BANG_HIST` and sets `PS2='%_> '` so any continuation prompt is nameable.

## Diagnostics now reported
On EXEC timeout and on every TAIL that still sees no sentinel, the response appends `[diagnostics]`:
- zsh PS2 prompt at buffer end (`quote> dquote> bquote> heredoc> cmdsubst>` plus the other zsh continuation tokens),
- `suspended (tty input)` / `suspended (tty output)` job notices,
- BEL bytes (over-long tty input line),
- tty modes that differ from the post-init `stty -a` snapshot (e.g. `-icanon (was icanon)`), via `stty -a -f <slave>`,
- foreground process group not being the shell,
- always: the last 500 bytes of the raw pty buffer, escaped (control bytes visible).
Watchdog: if the buffer sits at a PS2 prompt for 2s with no sentinel, send Ctrl-C, an empty line (zsh only abandons the
half-read command after the next line) and a fresh sentinel; the call returns with
`[command was incomplete (unbalanced quote/heredoc) — interrupted with Ctrl-C]`. Applies to EXEC and TAIL.

## Tests
`typed_input_hazards_no_longer_hang`, `replay_hang{1,2}_*` (5 replays), `echo_pid_bang_in_quotes`, `overlong_single_line`,
`ps2_prompt_detected_only_at_buffer_end`, `job_control_notices_are_named`, `tty_mode_diff_names_changed_flags`,
`raw_tail_keeps_control_bytes_visible_and_bounded`, `watchdog_breaks_out_of_ps2_prompt`, `watchdog_breaks_out_of_heredoc`,
`pending_diagnostics_name_ps2_prompt`, `timeout_diagnostics_report_tty_mode_change_and_raw_tail`,
`job_control_notice_is_named_on_timeout`. Full `cargo test --release` (1212 pass) and `cargo clippy --release` clean.
Replays skip if the pm-keepalive/pm-permui worktrees are absent.
