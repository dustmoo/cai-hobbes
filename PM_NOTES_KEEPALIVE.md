# Prompt-cache keep-alive: PM notes

Branch `pm/cache-keepalive` (off `feat/anthropic-prompt-caching` @5b01f21). Not merged, not pushed, no app launched, no `build_pro.sh`.

## What I built
- **`src/llm/claude/keepalive.rs`** (new; claude.rs only gained `pub mod`, a begin/finish/abort guard, and a `turn_start` flag from `prepare_request`):
  - Per-session registry with a generation counter; a new real request, tab close, session delete or quit supersedes the pending timer.
  - After each successful cached Claude request the timer is armed at `keepalive_refresh_secs` (default 255s, clamped to TTL minus 15s), counted from the request's *start* (docs: the TTL runs from request start).
  - Pure `gate()` for every condition in the spec: enabled, 5m TTL, request known, not in flight, not dead, not quitting, idle < max idle, prefix >= min tokens (last request's read+write), daily spend < cap.
  - Ping = the last real request body with `stream:false`, `max_tokens:0`, `diagnostics` removed. Tools/system/messages/thinking/tool_choice are untouched (the real tail is kept, so the request stays valid). Falls back to `max_tokens:1` (and remembers) if the API rejects 0.
  - A hit re-arms. A ping that writes >= 4096 tokens and more than it read marks the session dead (logged). Any ping failure stops that tab.
  - Ping usage goes over a channel to the app, which records it in `UsageLog` (a Signal can't be written from a background task); today's spend is seeded from the log at startup.
- **Telemetry** (`usage_log.rs`): `UsageLogEntry.kind` (`turn`|`keepalive`, default turn) and an optional `cache` block. Turn entries get idle gap, first-request hit/miss tokens, whether a keep-alive bridged the gap, pings in the gap, within-turn read/write totals, plus prices and TTL. Ping entries get prefix tokens, idle secs, read/write tokens, cost.
- **Settings** (General tab, under the caching toggle, shown for the 5m TTL): "Keep cache warm while I'm away" (default on), stop after 15/30/60 min idle, daily cap $0.50/1/2/5. Min-prefix (8k) and refresh seconds are settings.json only.
- **`scripts/cache_report.py`**: hit rate (turn-first vs within-turn), idle-gap histogram, keep-alive spend vs estimated write savings, 1h what-if, recommendation. Tested only on a hand-made sample file; there is no real telemetry yet.
- Stash `stash@{0}` was reused for the config, usage-log and claude.rs hook ideas; its keepalive module did not exist, so it was written fresh. The stash is still there, untouched.

## Doc findings (re-verified today)
Source: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
- A cache read refreshes the TTL at no extra charge; the lifetime is measured from the *start* of the request.
- `max_tokens: 0` pre-warms: reads the prompt, writes the cache at breakpoints, returns an empty `content`, `stop_reason: "max_tokens"`, full `usage`, zero output billed. A write is billed if the prefix wasn't cached.
- `max_tokens: 0` is rejected with `invalid_request_error` for `stream:true`, `thinking.type:"enabled"`, `output_config.format`, and `tool_choice` of type `tool` or `any`. It is also rejected in Message Batches.
- **Adaptive thinking is not mentioned** in the restriction list (only `"enabled"`). Hobbes only ever sends `adaptive`, so max_tokens 0 *should* work, but this is unverified against the live API. The fallback to `max_tokens: 1` covers a rejection; I haven't confirmed that 1 is accepted with adaptive thinking either.
- Cache-key inputs: tool definitions, web-search/citations toggles, speed, thinking params, and effort invalidate system/message caches; `tool_choice` and images invalidate only the message cache. The doc says model is not a listed invalidator, though it must obviously match for a hit. `max_tokens` and `stream` are not listed, which is what makes the ping safe.

## Open questions
1. First real run: does `max_tokens:0` succeed with adaptive thinking? Look for "max_tokens 0 rejected" in hobbes.log and a keep-alive entry in usage_log.json.
2. Is `stream:false` with the same body really a hit against a streamed request's entry? The docs imply yes. The first ping's `cache_read_input_tokens` will confirm.
3. Always-on models send `display: "summarized"`; the ping keeps it (byte-identical). Fine, but it was not tested live.
4. A tab closed while its stream is still running: `cancel_session` removes the entry, so no ping follows. A tab hidden but still open keeps pinging until max idle (per spec: "background" is read as closed/ended).
5. The 1h-vs-keepalive recommendation in the report is a rough model (assumes one price for the whole log). Treat it as a prompt for judgment, not a verdict.
6. Quit hook is the menu Quit action only; the window-close path isn't hooked, but pings die with the process anyway.

## Verification (debug builds only, shared target dir, 4 jobs)
- `cargo test`: 1198 passed, 0 failed, 1 ignored. Seven new keepalive tests: each gate condition, refresh clamp, turn-start detection, ping body differs only in stream/max_tokens/diagnostics, ping fingerprint equals the real request's (reuses `fingerprint_request`), and timer supersession/cancel.
- `cargo clippy`: no warnings.
- Not tested live: no real API call, per instructions.
