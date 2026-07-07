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

**Status:** committed as `apps/sandbox_shim/verify-sandbox.ps1` and ALL GREEN.
Note: the original check #1 (a single-line-piped JSON-RPC `initialize`) was
dropped — it returns no result even UNSANDBOXED (stdin EOF races startup). It's
replaced by a deterministic node stdin→stdout echo plus an npx "server starts"
smoke. Full request/response is validated by the Task 1 e2e test instead.

---

## Task 1 — In-app end-to-end validation (HIGHEST priority)

**Core transport risk: RETIRED.** A headless e2e test now drives the REAL app
path — `wrap_command` + the child env + the rmcp `TokioChildProcess` transport —
against a sandboxed `npx -y @modelcontextprotocol/server-everything`, and passes:
`serve`/initialize ✓, `list_tools` ✓ (13 tools), `call_tool(get-sum, 2, 3)` → 5 ✓.
So the `npx→cmd→node` chain DOES carry a full request/response through the app's
transport. Test: `src/mcp/manager.rs` mod `win_sandbox_e2e` (Windows-only,
`#[ignore]`d). Run:

```text
cargo build -p hobbes_sandbox
cargo test --bin Hobbes -- --ignored win_sandbox_e2e
```

Getting there surfaced (and fixed) a real Windows env bug in the app — NOT the
shim (commit `c82b0cf`):
- The child `PATH` was `format!("{}:{}", get_sane_path(), current_path)` — Unix
  dirs colon-joined, which corrupts Windows drive letters. Now platform-aware
  (`get_sane_path` Windows branch + a `join_paths` helper; all 5 compose sites).
- `apply_env_policy` `env_clear()`s registry-install children (so they can't
  inherit dotenv secrets); on Windows that also stripped `SystemRoot`/`ComSpec`/
  `PATHEXT`, and node crashed at startup (`Assertion failed: ncrypto::CSPRNG`).
  It now re-adds a non-secret Windows system-var allowlist after clearing.

**Remaining (needs the GUI — cannot be done headlessly): the human click-through.**
1. Build: `.\scripts\build_windows.ps1` (builds `hobbes.exe` + `hobbes-sandbox.exe`
   side by side; the installer `.iss` also ships the shim next to the app exe).
2. Launch Hobbes → marketplace → search "everything" → install the
   modelcontextprotocol one (`npx -y @modelcontextprotocol/server-everything`).
3. Ask the AI to invoke a tool and confirm a result. Current tool names (the
   server renamed them): **`get-sum`** (adds two numbers → 5), **`echo`**,
   **`get-env`** (dumps the env the server sees — use it to eyeball that
   `TEMP`/`HOME`/`USERPROFILE` point into `C:\HobbesSandbox\...` and the real
   profile is absent).

**Definition of done:** a registry server installed in the real Hobbes app runs
sandboxed AND its tools are callable by the AI. The transport half is proven; the
GUI click-through is the last confirmation.

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

## Open design decision — RESOLVED: `--allow` under the profile works

The earlier premise ("`--allow` paths under `C:\Users` are unreachable without
elevation") was **wrong** — it predated the bypass-traverse-checking finding.
Verified empirically (`hobbes-sandbox --allow C:\Users\<u>\proj -- node -e read`):
without `--allow` a profile file is DENIED; with `--allow` it READs. The broker
runs as the user, so it owns the user's own dirs and the existing
`grant_access` puts the package-SID gate on the `--allow` leaf; bypass-traverse
means `C:\Users` itself never needs to be traversable. So filesystem-type MCP
servers pointed at a home-dir project folder **do** work on Windows, matching
macOS/Linux. No proxy needed. The only remaining question is posture (should
unvetted registry servers reach user-granted folders?) — and macOS/Linux already
allow exactly that via `--allow`, so consistency says yes.

---

## Workflow

- Edit `apps/sandbox_shim/src/main.rs`, keep `cargo clippy -p hobbes_sandbox
  --target x86_64-pc-windows-msvc` clean, run Task 0's `verify-sandbox.ps1`
  after each change, commit + push to `feat/glama-registry-sandbox`.
- CI passing ≠ sandbox correct — CI never launches an AppContainer. The manual
  regression gate is the real check.
- Don't touch `src/mcp/sandbox.rs` macOS/Linux branches or the shared app crate.
```
