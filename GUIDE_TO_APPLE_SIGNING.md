# Guide to macOS App Signing & Distribution

> A practical guide for Hobbes (and similar macOS apps) to avoid the "bouncing app" and Transporter validation nightmares.

## TL;DR Decision Tree

```
Do you want to run LOCALLY for testing?
├── YES → Use DEVELOPMENT profile + DEVELOPMENT certificate
│         HOBBES_SIGNING_ID="Apple Development: ..." ./scripts/build_release.sh
│         (App will launch and prompt for biometrics)
│
└── NO, I want to upload to TestFlight/App Store
    └── Use DISTRIBUTION profile + DISTRIBUTION certificate
        ./scripts/build_release.sh  (uses defaults)
        (App will "bounce" if run locally - this is EXPECTED)
```

---

## The Three Pillars of macOS Signing

### 1. Certificates (in your Keychain)
These are cryptographic identities stored in **Keychain Access**.

| Certificate Type | Purpose | When to Use |
|------------------|---------|-------------|
| `Apple Development: NAME (ID)` | Local testing | Running `.app` directly on your Mac |
| `Apple Distribution: NAME (ID)` | App Store / TestFlight | Uploading via Transporter |
| `Developer ID Application: NAME (ID)` | Direct distribution | Notarization for downloads outside App Store |
| `3rd Party Mac Developer Installer: NAME (ID)` | Installer signing | Creating `.pkg` for App Store |
| `Developer ID Installer: NAME (ID)` | Installer signing | Creating `.pkg` for direct distribution |

**Check your certificates:**
```bash
security find-identity -v -p codesigning
```

### 2. Provisioning Profiles
These are XML files that link your **App ID**, **Certificates**, and **Entitlements**.

| Profile Type | Contains | Use Case |
|--------------|----------|----------|
| Development | Dev certificate, device list | Local testing |
| App Store (Distribution) | Distribution certificate | TestFlight / App Store |
| Developer ID | Developer ID certificate | Direct distribution |

**Decode a profile:**
```bash
security cms -D -i profile.provisionprofile > profile.plist
/usr/libexec/PlistBuddy -c "Print :Name" profile.plist
```

### 3. Entitlements
These are capabilities your app requests (sandbox, keychain, network, etc.).

**Critical Rule:** The entitlements in your `.entitlements` file must be a **subset** of what's allowed by the embedded provisioning profile.

---

## Common Errors & Fixes

### "Killed: 9" / App Bounces Immediately
**Cause:** App signed with Distribution certificate but run locally without going through App Store/TestFlight.

**Fix:** For local testing, use Development certificate:
```bash
HOBBES_SIGNING_ID="Apple Development: DUSTIN ALAN MOORE (4753E57CRM)" \
HOBBES_PROVISION_PROFILE="dev.provisionprofile" \
./scripts/build_release.sh
```

### "Invalid Provisioning Profile - Missing code-signing certificate"
**Cause:** The certificate embedded in the provisioning profile doesn't match any certificate in your Keychain, OR you have stale build artifacts.

**Fix:**
1. Clean the build:
   ```bash
   rm -rf target/dx/Hobbes/release/macos/Hobbes.app
   ```
2. Verify the profile matches your cert:
   ```bash
   # Extract cert fingerprint from profile
   security cms -D -i embedded.provisionprofile > /tmp/p.plist
   # Then decode the DeveloperCertificates data and compare to keychain
   ```
3. Rebuild with correct profile in place.

### "errSecMissingEntitlement" (-34018) at Runtime
**Cause:** App trying to use keychain access groups not allowed by profile.

**Fix:** Ensure `keychain-access-groups` in your entitlements file matches what's in the provisioning profile.

---

## Hobbes-Specific Configuration

### Files
| File | Purpose |
|------|---------|
| `embedded.provisionprofile` | Default profile for builds (should be Distribution for releases) |
| `dev.provisionprofile` | Development profile for local testing |
| `Hobbes.entitlements` | Entitlements for App Store builds (sandbox enabled) |
| `Hobbes.dev.entitlements` | Entitlements for development builds |

### Environment Variables
| Variable | Default | Description |
|----------|---------|-------------|
| `HOBBES_SIGNING_ID` | `Apple Distribution: DUSTIN ALAN MOORE (ABXVW6PWCW)` | Code signing identity |
| `HOBBES_PROVISION_PROFILE` | `./embedded.provisionprofile` | Path to provisioning profile |

### Quick Commands

**Build for App Store/TestFlight:**
```bash
# Ensure embedded.provisionprofile is the DISTRIBUTION profile
./scripts/build_release.sh && ./scripts/package_release.sh
# Upload target/dx/Hobbes/release/macos/Hobbes.pkg via Transporter
```

**Build for Local Testing:**
```bash
HOBBES_SIGNING_ID="Apple Development: DUSTIN ALAN MOORE (4753E57CRM)" \
HOBBES_PROVISION_PROFILE="dev.provisionprofile" \
./scripts/build_release.sh
# Run directly
./target/dx/Hobbes/release/macos/Hobbes.app/Contents/MacOS/Hobbes
```

---

## Checklist Before Upload

- [ ] `embedded.provisionprofile` is the **Distribution** profile (not dev)
- [ ] Built with **Apple Distribution** certificate (not Development)
- [ ] `Hobbes.entitlements` has sandbox enabled (`com.apple.security.app-sandbox` = true)
- [ ] Deleted old `.app` bundle before rebuilding (to avoid stale artifacts)
- [ ] Verified with:
  ```bash
  codesign -dvvv target/dx/Hobbes/release/macos/Hobbes.app 2>&1 | grep Authority
  # Should show "Apple Distribution: ..."
  ```

---

## Helpful Debugging Commands

```bash
# List all signing identities
security find-identity -v -p codesigning

# Decode provisioning profile
security cms -D -i some.provisionprofile > decoded.plist

# Check what's embedded in built app
security cms -D -i Hobbes.app/Contents/embedded.provisionprofile

# Verify code signature
codesign -dvvv Hobbes.app

# Check entitlements of signed app
codesign -d --entitlements - Hobbes.app
```

---

## Remember

1. **Distribution = Can't run locally** (macOS kills it immediately)
2. **Development = Can't upload** (Transporter rejects it)
3. **Always delete the old `.app` before switching profiles** (build cache doesn't always update everything)
4. **Profile must contain the certificate you're signing with** (or you get "Missing code-signing certificate")

---

*Last updated: 2026-01-04*
