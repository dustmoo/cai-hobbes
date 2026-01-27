# Security Policy

## Credential Management Mandate
**Strict Notification:** All credentials, keys, and secrets must be managed via the System Keychain.
**Forbidden Files:** The following file types are strictly prohibited from the repository, even in `.gitignore`:
- `*.p12` (Identity Certificates)
- `*.key` (Private Keys)
- `*.pem` (Private Keys/Certs)
- `*.certSigningRequest` (CSRs)
- `*.provisionprofile` (Provisioning Profiles - Local Only)

## Incident Response Protocol
If strict credentials are detected in the repository history:
1.  **Immediate Revocation:** The credential is legally compromised. Revoke it at the provider source (e.g., Apple Developer Portal) immediately.
2.  **History Scrub:** Perform a `git filter-branch` or `git filter-repo` scrub to remove artifacts.
3.  **Rotation:** Re-issue new credentials from a clean state.

## Local Primacy Strategy
Hobbes enforces "Local Primacy" for all sensitive operations. Keys are generated locally, stored in Keychain, and accessed via the `keychain_ffi` bridge. They never touch the file system or git index.
