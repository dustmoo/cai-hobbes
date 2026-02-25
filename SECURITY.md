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

## Regulatory & Territorial Compliance

Hobbes is developed in the State of Washington, USA. It operates on a **BYOK (Bring Your Own Key)** architecture: the software itself performs no inference, stores no model weights, and makes no AI decisions. The user is the sole **Deployer** of any connected AI models.

**EU AI Act (Regulation (EU) 2024/1689):** Hobbes is not intended for use within the European Union. We do not market, distribute, or support this software in the EU. Users who access Hobbes from within the EU do so at their own risk and assume full responsibility for compliance with the EU AI Act, GDPR, and all applicable local regulations. See [`README.md`](README.md) for the full territorial use restriction.
