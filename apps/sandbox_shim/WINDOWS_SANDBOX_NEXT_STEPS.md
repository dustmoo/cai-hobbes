# Windows sandbox — next steps (scoped tasks)

Context: `apps/sandbox_shim/src/main.rs` (the `hobbes-sandbox.exe` AppContainer
broker) works end-to-end from the shim CLI. See `WINDOWS_SANDBOX_FINDINGS.md`
for the access model. These tasks take it from "shim proven" to "landable."

**The one rule that governs everything below** (from the findings): an
AppContainer can reach an object only if its DACL grants **both**
1. a **normal token SID** the container has (`BUILTIN\Users`, `Authenticated
   Users`, or the user's own SID) for the right (RX to execute, M/F to write), **and**
2. the **app-container gate**: `ALL APPLICATION PACKAGES` (S-1-15-2-1) **or**
   the container's own **package SID**.

Effective container access is the **intersection** of (1) and (2). Every
lockdown step must preserve *both halves* for paths the tool legitimately needs,
or it breaks. This is the thing to not get wrong.

---

## Task 0 — Regression gate (do this FIRST, run after every change)

Before touching anything, save this as `apps/sandbox_shim/verify-sandbox.ps1`
and confirm it passes on the current (known-good) build. Then run it again after
**each** hardening step below. If any check flips, you broke reachability — stop
and fix before continuing.

```powershell
# verify-sandbox.ps1 — run from repo root in a NATIVE PowerShell (not Git Bash)
$ErrorActionPreference = 'Continue'
$shim = ".\target\debug\hobbes-sandbox.exe"
cargo build -p hobbes_sandbox 2>&1 | Out-Null
$fail = 0

# 1. FUNCTIONAL: npx MCP server starts and answers the initialize handshake
$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}'
$out = $init | & $shim --name verify --net -- npx -y "@modelcontextprotocol/server-everything" 2>$null
if ($out -match '"result"') { "PASS  functional: server responded" }
else { "FAIL  functional: no JSON-RPC result"; $fail++ }

# 2. SECURITY: real user profile is NOT readable from inside the container
$deny = & $shim --name verify --net -- cmd /c "dir C:\Users\$env:USERNAME\Desktop" 2>&1
if ($deny -match 'Access is denied|denied') { "PASS  security: profile read denied" }
else { "FAIL  security: profile READABLE from container"; $fail++ }

# 3. NETWORK: allowed with --net, blocked without
$on  = & $shim --name verify --net -- curl.exe -s -o NUL -w "%{http_code}" https://example.com 2>$null
$off = & $shim --name verify -- curl.exe -s -m 8 -o NUL -w "%{http_code}" https://example.com 2>$null
if ($on -eq '200') { "PASS  network: --net reaches internet" } else { "FAIL  network: --net blocked ($on)"; $fail++ }
if ($off -ne '200') { "PASS  network: no --net blocks internet" } else { "FAIL  network: leak without --net"; $fail++ }

if ($fail -eq 0) { "`nALL GREEN" } else { "`n$fail CHECK(S) FAILED" }
```

Commit `verify-sandbox.ps1` so it's the durable regression gate.

---

## Task 1 — In-app end-to-end validation (HIGHEST priority)

Everything so far is the shim CLI. Prove the **real app path** works: the
manager launches servers through `wrap_command` (`src/mcp/sandbox.rs`, Windows
branch → the shim), and the AI must be able to *call the server's tools*, not
just see it start.

1. Build the app + shim: `.\scripts\build_windows.ps1` (produces both
   `hobbes.exe` and `hobbes-sandbox.exe` side by side — the shim must sit next
   to the app exe; `shim_path()` looks there).
2. Launch Hobbes, install a Glama registry server (an npx-based one), and
   confirm: it starts sandboxed, and the AI can successfully **invoke one of its
   tools** and get a result back.
3. The key risk (findings §"stdin→server response"): the `npx → cmd → node`
   stdio chain may not cleanly carry a full request/response through the app's
   child-process transport, even though a manual `initialize` works. If tool
   calls hang or truncate, that's this issue.
   - If it fails: the fix direction is to run servers as `node <entry>` directly
     at the MCP layer instead of via `npx`/`cmd`. **Confirm the failure in-app
     first** — don't re-architecture on suspicion.

**Definition of done:** a registry server installed in the real Hobbes app runs
sandboxed AND its tools are callable by the AI. This is the gate for "Windows
sandbox done."

---

## Task 2 — ACL hardening (do before public release; NOT a data-exposure blocker)

Today's `C:\HobbesSandbox` inherits `Authenticated Users:(M)` from the `C:\`
root. User data confinement already holds (Task 0 check #2 proves it); this
closes two lesser gaps: other-user code tampering with the shared toolchain, and
weak server-to-server isolation. By the intersection rule a *container* already
can't write the toolchain (gate caps it at RX), so this is defense-in-depth.

Harden incrementally, **running Task 0 after each sub-step**. The target DACLs:

### 2a. Shared toolchain copy (`C:\HobbesSandbox\toolchain\<name>`)
Read-only to containers, writable only by the provisioning user.
- Strip inheritance (protected DACL).
- `SYSTEM:(F)`, `BUILTIN\Administrators:(F)` — maintenance.
- **Owner / the provisioning user:(F)** — so the broker can refresh copies.
  (A container can't abuse this Full: its access = user-SID(F) ∩ gate(RX) = RX.)
- **`ALL APPLICATION PACKAGES:(RX)`** — the gate + read/execute.
- **Do NOT** grant `Authenticated Users` anything (that's the ACE being removed).

Invariant to preserve: the tree must still expose **a normal-SID RX** (the
Owner/user ACE) **and** the **gate (ALL APP PACKAGES:RX)**. Drop either and
in-container `node` stops executing → Task 0 check #1 fails.

### 2b. Per-container state dirs (`C:\HobbesSandbox\<container>\{npm-cache,tmp,...}`)
Writable by *that* container only.
- Strip inheritance on the per-container dir.
- `SYSTEM:(F)`, `Administrators:(F)`, provisioning user:(F).
- **A normal SID with write** the container has — grant `BUILTIN\Users:(M)`
  (inheritable) **explicitly** (this replaces the inherited `Authenticated
  Users:(M)` you're removing).
- **The container's own package SID:(F)** (inheritable) — the gate.
- Because the gate is the *specific* package SID (not ALL APP PACKAGES), other
  containers fail the gate check on this dir → real per-container isolation.

Invariant to preserve: the container must keep **normal-SID write (Users:M)**
**and** its **package-SID gate** here, or cache writes break → Task 0 check #1
fails with a write "Access is denied".

### 2c. State root (`C:\HobbesSandbox` itself)
- Strip the inherited `Authenticated Users:(M)`.
- Leave it **traversable** by containers (they must pass through it to reach
  their leaves): grant `ALL APPLICATION PACKAGES:(RX)` on the root dir only
  (non-inheritable, so it doesn't re-open the children). Keep SYSTEM/Admins/user
  Full for maintenance.

**Implementation note:** the shim's `grant_ace` already takes an inheritance
flag; extend it to also *protect* the DACL (disable inheritance) when hardening —
`SetNamedSecurityInfoW` with `PROTECTED_DACL_SECURITY_INFORMATION`. Do the
protect+regrant in one pass per dir so there's never a window with no ACEs.

**Order matters:** harden the deepest leaves first, root last, and run Task 0
between each — that way if a step breaks reachability you know exactly which ACL
did it.

---

## Task 3 — State-root cleanup (do with Task 2)

Nothing removes `C:\HobbesSandbox\<container>` dirs, so they accumulate per
installed server, plus full Node copies under `toolchain\`.
- On server removal, delete its per-container dir. Wire this into the existing
  remove handler in `src/components/installed_mcps.rs` (same place that already
  deletes keychain secrets and `secret_env` on removal) — pass the sanitized
  container name so it can `rm -rf C:\HobbesSandbox\<name>`.
- Leave the shared `toolchain\` copies (reused across servers; the staleness
  stamp refreshes them). Optionally prune on app uninstall.
- Heads-up: a full `node.exe` copied to `C:\HobbesSandbox` may trip AV
  heuristics. Worth a note in release docs / an AV-exclusion suggestion.

---

## Task 4 — Provisioning cost (DEFER; polish only)

First sandboxed launch pays a one-time ~4s Node copy (cached, staleness-stamped
per Node version). Nice-to-have: provision once at Hobbes startup or in a
background task so the first server launch isn't slow. Not blocking.

---

## Open design decision (not a task — needs a call)

`--allow` paths under `C:\Users\<user>` are **unreachable** by an AppContainer
(the profile isn't traversable and can't be made so without elevation). So a
filesystem-type MCP server pointed at a project folder in the user's home dir
will not work on Windows. Two paths:
- **Accept it** — registry servers get no user-file access (arguably the safe
  default for unvetted code), documented as a known platform limitation.
- **Build a proxy later** — the broker mediates specific allowed paths (copy-in
  / copy-out or a mapped location under `C:\HobbesSandbox`).

Decide explicitly before shipping so it's not discovered in the field.

---

## Workflow

- Edit `apps/sandbox_shim/src/main.rs`, keep `cargo clippy -p hobbes_sandbox
  --target x86_64-pc-windows-msvc` clean, run Task 0's `verify-sandbox.ps1`
  after each change, commit + push to `feat/glama-registry-sandbox`.
- CI passing ≠ sandbox correct — CI never launches an AppContainer. The manual
  regression gate is the real check.
- Don't touch `src/mcp/sandbox.rs` macOS/Linux branches or the shared app crate.
```
