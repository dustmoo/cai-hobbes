# Certificate Chain Issue Investigation

**Date**: 2025-12-20
**Context**: macOS Keychain Biometric Implementation

## Issue
When trying to sign the application with `codesign`, we encounter:
```
errSecInternalComponent
unable to build chain to self-signed root
```

## Observations
1. **Certificate exists**: "Apple Development: dustin@tulipvalleytech.com (4753E57CRM)" is present in the keychain.
2. **Marked Trusted**: Keychain Access shows it as trusted.
3. **Intermediates Installed**: 
   - Apple Worldwide Developer Relations Certification Authority (G3) is installed.
   - Apple Root CA is installed.
   - Both were manually installed to System keychain and verified.
4. **Ad-hoc Signing Works**: `codesign --sign -` works perfectly and enables the keychain functionality for local dev.

## Workaround
Currently using **ad-hoc signing** in `dev.sh`.

```bash
# dev.sh
IDENTITY="-"
codesign --force --deep --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" "$BINARY"
```

## Resolution Plan
To fix proper Apple Development signing:
1. Revoke the current certificate in Xcode.
2. Delete the certificate and private key from Keychain Access.
3. Create a fresh certificate in Xcode.
4. Restart macOS to flush `secd` and keychain caches.
