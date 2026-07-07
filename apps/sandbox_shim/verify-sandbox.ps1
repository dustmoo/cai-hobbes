# verify-sandbox.ps1 — run from repo root in a NATIVE PowerShell (not Git Bash)
$ErrorActionPreference = 'Continue'
$shim = ".\target\debug\hobbes-sandbox.exe"
cargo build -p hobbes_sandbox 2>&1 | Out-Null
$fail = 0

# 1a. FUNCTIONAL (deterministic): node runs in-container and stdio passes
#     through the sandbox + cmd + node chain. This is the reliable signal.
#     (A full JSON-RPC initialize round-trip is NOT used here: a single-line
#     piped request doesn't elicit a response even UNSANDBOXED — stdin EOF races
#     server startup / the npx→cmd→node stdio chain. Validate real request/
#     response in-app per Task 1, not from a shell pipe.)
$echo = "MARKER-$PID" | & $shim --name verify -- node -e "process.stdin.pipe(process.stdout)" 2>$null
if ($echo -match "MARKER-$PID") { "PASS  functional: node runs, stdio passes through" }
else { "FAIL  functional: node/stdio broken"; $fail++ }

# 1b. FUNCTIONAL: an npx MCP server provisions its toolchain and starts.
#     server-everything prints this banner to stderr once it is up.
$srv = $null | & $shim --name verify --net -- npx -y "@modelcontextprotocol/server-everything" 2>&1
if ($srv -match 'Starting default \(STDIO\) server') { "PASS  functional: npx MCP server starts" }
else { "FAIL  functional: npx server did not start"; $fail++ }

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
exit $fail
