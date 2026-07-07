# Windows AppContainer sandbox — investigation findings (2026-07-06)

Reproduce with: `hobbes-sandbox.exe` built via `cargo build -p hobbes_sandbox`.
Debug env vars added during investigation (gated, harmless):
`HOBBES_SANDBOX_DEBUG` (print SID/cmdline/exit), `HOBBES_SANDBOX_NOCMD`
(skip the cmd.exe wrapper), `HOBBES_SANDBOX_WORKDIR` (override child cwd).

**Test from a NATIVE console (PowerShell), never Git Bash** — Git Bash mangles
`/c` → `C:/` and resolves the wrong (Git) tools. This wasted an earlier session.

## Bugs found

1. **resolve_command_path picked the extensionless Unix script.** Node ships
   `npx`, `npx.cmd`, `npx.ps1` side by side on Windows; the resolver matched the
   bare `npx` (a Unix shell script) before trying PATHEXT, and cmd can't execute
   it → "Access is denied". FIXED: only ever append a PATHEXT extension (mirror
   cmd semantics), never the extensionless file.

2. **Node crashes on `lstat 'C:\'`.** Node's module resolution does
   `realpathSync` on the entry script, walking every path component up to the
   drive root. The container can *traverse* C:\ (bypass-traverse) but cannot
   *open/stat C:\ root itself* → `EPERM: lstat 'C:\'`. FIXED by
   `NODE_OPTIONS=--preserve-symlinks-main --preserve-symlinks` (skips realpath).
   Verified: parent node then runs.

## Proven AppContainer facts (this machine, Node v24.18.0 in Program Files)

- The container token does **not** contain `ALL APPLICATION PACKAGES` (S-1-15-2-1).
  Proof: a file granted *only* to S-1-15-2-1 is **denied** to the container.
  ⇒ Every `ALL APPLICATION PACKAGES` ACE we or Windows set is **useless** to it.
  The token has BUILTIN\Users (enabled), Authenticated Users, the user SID, the
  container **package SID**, at **Low** integrity.
- Effective access model observed: an object is reachable only if its DACL grants
  one of the token's normal SIDs (Users/user) **AND** the object is
  "package-accessible". System dirs (System32, Program Files-proper) satisfy this;
  the user profile does not → **profile reads are correctly DENIED** (the security
  goal holds).
- **Read-confinement works**: `dir C:\Users\dustm\Desktop` from the container → denied.
- `C:\Program Files\nodejs` has a **replaced, non-inheriting ACL** (Users:RX,
  Admins/SYSTEM only) — the node installer stripped the inherited
  ALL-APPLICATION-PACKAGES ACE that C:\Program Files otherwise carries. It is
  SYSTEM-owned, so a non-elevated user cannot add an ACE.

## What WORKS

- **Broker** (full user token) launches any exe it can read into the container:
  bare `node.exe --version` from Program Files → `v24.18.0`. ✓
- **In-container node executes a broker-owned copy** of node.exe placed in
  `C:\HobbesSandbox\...` (granted the container **package SID** + Users), and that
  node **spawns a child node** successfully (with NODE_OPTIONS set). ✓ ← key primitive
- **Direct CreateProcess** (no cmd) of a package-accessible node copy works.

## What does NOT work

- **cmd.exe inside the container cannot launch a tool from a broker-owned dir**
  (even with ALL-APP-PACKAGES + package-SID + Users granted). It also can't
  `dir` those paths, though `type <file>` (direct read) works. cmd appears to
  pre-validate the target path via a directory-query/enumeration op the container
  can't perform; direct CreateProcess (node/broker) avoids it.
- Therefore **npx/npm break**: npx downloads the package fine, then shells out
  through cmd.exe to run the server's `.cmd` bin shim → `'"node"' is not
  recognized` / "Access is denied". The npm-internal cmd usage is the wall.

## The refined access model (what actually decides reachability)

An AppContainer created via `CreateAppContainerProfile` (no capabilities) gets a
token WITHOUT `ALL APPLICATION PACKAGES`, but WITH `BUILTIN\Users` (enabled) at
Low integrity. An object is reachable iff its DACL grants **both**:
1. a normal token SID for the right (e.g. `Users:(RX)`), **and**
2. the app-container *gate*: `ALL APPLICATION PACKAGES` **or** the container's
   package SID.

This explains everything:
- User profile → grants the user but NOT the gate → **denied** (security holds).
- System32 / Program Files-proper → grant Users + ALL-APP-PACKAGES → reachable.
- `C:\Program Files\nodejs` → Users but the installer stripped the gate → **not**
  reachable, and SYSTEM-owned so a non-elevated broker can't re-add the gate.
- A broker-owned copy granted `ALL APPLICATION PACKAGES:(RX)` (gate) + inherited
  `Users:(RX)` (right) → reachable, read-only, and universal (one grant serves
  every container). This is the fix.

Two more cmd-specific quirks:
- cmd can execute/enumerate a granted dir only when the tool is found by **bare
  name via PATH** — an **explicit full path** trips cmd's path canonicalisation,
  which needs a directory query the container can't do ("Access is denied").
- cmd's in-container PATH search silently skips dirs the container can't reach,
  so the copy must be on PATH *and* gated as above.

## Implemented solution (node-direct via a provisioned toolchain)

The shim now:
1. Copies the tool's toolchain dir (e.g. `nodejs`) to
   `%SystemDrive%\HobbesSandbox\toolchain\<name>` (idempotent, staleness-stamped)
   and grants it `ALL APPLICATION PACKAGES:(RX)`.
2. Prepends that copy to the child PATH and invokes the tool by **bare name**.
3. Sets `NODE_OPTIONS=--preserve-symlinks-main --preserve-symlinks` and pins
   npm's cache/config/prefix + `HOME`/`USERPROFILE`/`APPDATA` into the sandbox.

Verified end-to-end (native console): `npx -y @modelcontextprotocol/server-everything`
starts the MCP server (exit 0); stdin/stdout pass through; `--net` gives HTTP 200
and its absence blocks the network; user-profile reads are denied.

Known cosmetic: one non-fatal "Access is denied." during `npx -y <pkg>` (npm's
bin-linking attempts a symlink the container can't create, then falls back to a
cmd shim). The server starts regardless.

## Follow-up / hardening (not blocking)
- The state root inherits `Authenticated Users:(M)` from `C:\` root, so the
  provisioned toolchain and per-container dirs are technically writable by any
  container (weak server-to-server isolation; the *user's* data is still safe).
  Harden by stripping inheritance on the state root and granting per-container
  package SIDs explicitly + a read-only toolchain ACL.
- First launch pays a one-time toolchain copy (~4s for Node). Consider
  provisioning once at Hobbes startup instead of lazily in the shim.
- The stdin→server response over `npx`-via-cmd is a separate Windows/npx concern
  (reproduces WITHOUT the sandbox); Hobbes may prefer running servers as
  `node <entry>` directly at the MCP layer.
