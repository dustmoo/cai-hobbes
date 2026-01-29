#!/bin/bash
# Build, patch, and sign the Hobbes release binary for macOS
# This script ensures proper Info.plist configuration and code signing for biometric keychain access.
set -e

APP_PATH="target/dx/Hobbes/release/macos/Hobbes.app"
BINARY="$APP_PATH/Contents/MacOS/Hobbes"
PLIST="$APP_PATH/Contents/Info.plist"
ENTITLEMENTS="Hobbes.entitlements"
# For App Store distribution, use "3rd Party Mac Developer Application" or "Apple Distribution"
# For local development/testing, use "Apple Development"
IDENTITY="${HOBBES_SIGNING_ID:-Apple Distribution: DUSTIN ALAN MOORE (ABXVW6PWCW)}"
export MACOSX_DEPLOYMENT_TARGET=12.0

# Auto-resolve identity to a fingerprint to avoid "ambiguous" errors if duplicates exist
# security find-identity -v only lists VALID identities, effectively ignoring revoked ones
# explicitly exclude REVOKED ones just in case they show up in the valid list (occasional macOS behavior)
RESOLVED_FINGERPRINT=$(security find-identity -v -p codesigning | grep "$IDENTITY" | grep -v "CSSMERR_TP_CERT_REVOKED" | head -1 | awk '{print $2}')
if [ -n "$RESOLVED_FINGERPRINT" ]; then
    echo "  ✅ Resolved '$IDENTITY' to fingerprint: $RESOLVED_FINGERPRINT"
    IDENTITY="$RESOLVED_FINGERPRINT"
fi

echo "=== Building Release App Package ==="
dx build --release

# Fail-safe: Ensure binary exists in bundle
if [ ! -f "$BINARY" ]; then
    echo "⚠️  Binary missing in bundle! Attempting manual copy..."
    # Fallback path for the raw binary
    RAW_BINARY="target/release/Hobbes"
    if [ -f "$RAW_BINARY" ]; then
        cp "$RAW_BINARY" "$BINARY"
        chmod +x "$BINARY"
        echo "✅ Manually copied binary to bundle."
    else
        echo "❌ Critical Error: Could not find compiled binary at $RAW_BINARY"
        exit 1
    fi
fi

echo ""
echo "=== Installing Icon (Standard) ==="
ICON_SOURCE="assets/icon.icns"
if [ -f "$ICON_SOURCE" ]; then
    cp "$ICON_SOURCE" "$APP_PATH/Contents/Resources/icon.icns"
    echo "  ✅ Copied standard icon to bundle"
else
    echo "  ⚠️  Standard icon missing at $ICON_SOURCE"
fi

# Patch version in plist (Dioxus 0.6 workaround)
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [ -f "$PLIST" ]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $VERSION" "$PLIST"
    
    BUILD_VERSION=$(echo "$VERSION" | sed 's/-.*//') 
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_VERSION" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $BUILD_VERSION" "$PLIST"
    echo "  ✅ Patched version to $VERSION"
fi

# Wait for the binary to be fully written (dx build may return before linking completes)
echo "=== Waiting for binary to stabilize ==="
sleep 2  # Initial wait for filesystem sync

# Poll until the binary hasn't changed for 1 second
PREV_SIZE=0
STABLE_COUNT=0
while [ $STABLE_COUNT -lt 3 ]; do
    if [ ! -f "$BINARY" ]; then
        echo "  Waiting for binary to appear..."
        sleep 1
        continue
    fi
    
    CURR_SIZE=$(stat -f%z "$BINARY" 2>/dev/null || echo "0")
    if [ "$CURR_SIZE" = "$PREV_SIZE" ] && [ "$CURR_SIZE" != "0" ]; then
        STABLE_COUNT=$((STABLE_COUNT + 1))
        echo "  Binary stable check $STABLE_COUNT/3..."
    else
        STABLE_COUNT=0
        PREV_SIZE=$CURR_SIZE
    fi
    sleep 0.5
done
echo "  ✅ Binary is stable: $(du -h "$BINARY" | cut -f1)"

echo ""
echo "=== Patching Info.plist ==="
# Add NSFaceIDUsageDescription if missing (required for biometric prompts)
if ! /usr/libexec/PlistBuddy -c "Print :NSFaceIDUsageDescription" "$PLIST" 2>/dev/null; then
    /usr/libexec/PlistBuddy -c "Add :NSFaceIDUsageDescription string 'Hobbes uses Touch ID to securely access your API keys in the Keychain.'" "$PLIST"
    echo "  ✅ Added NSFaceIDUsageDescription"
else
    echo "  ⏭️ NSFaceIDUsageDescription already present"
fi

# Set LSMinimumSystemVersion to match MACOSX_DEPLOYMENT_TARGET (required for arm64-only builds)
/usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion 12.0" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :LSMinimumSystemVersion string 12.0" "$PLIST"
echo "  ✅ Set LSMinimumSystemVersion to 12.0"

# Add attribution/copyright (applies to both Store and Pro)
/usr/libexec/PlistBuddy -c "Set :NSHumanReadableCopyright Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families." "$PLIST" 2>/dev/null || \
/usr/libexec/PlistBuddy -c "Add :NSHumanReadableCopyright string Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families." "$PLIST"
echo "  ✅ Added attribution/copyright"

echo ""
echo "=== Embedding Provisioning Profile ==="
# For Store builds, automatically use dist.provisionprofile
# This eliminates the need to manually run switch_profile.sh before building
DIST_PROFILE="./dist.provisionprofile"
PROVISIONING_PROFILE="${HOBBES_PROVISION_PROFILE:-$DIST_PROFILE}"

if [ "$CI" = "true" ]; then
    echo "  ⚠️  CI Mode: Skipping Provisioning Profile embedding."
elif [ -f "$PROVISIONING_PROFILE" ]; then
    cp "$PROVISIONING_PROFILE" "$APP_PATH/Contents/embedded.provisionprofile"
    echo "  ✅ Embedded provisioning profile from: $PROVISIONING_PROFILE"
elif [ -f "./embedded.provisionprofile" ]; then
    # Fallback to existing embedded profile if dist.provisionprofile doesn't exist
    cp "./embedded.provisionprofile" "$APP_PATH/Contents/embedded.provisionprofile"
    echo "  ✅ Embedded provisioning profile from: ./embedded.provisionprofile"
else
    echo "  ❌ Error: No provisioning profile found!"
    echo "     Please ensure 'dist.provisionprofile' is in the project root."
    exit 1
fi

echo ""
echo "=== Cleaning extended attributes ==="
xattr -cr "$APP_PATH"
echo "  ✅ Cleaned xattrs"

echo ""
echo "=== Code Signing ==="
if [ "$CI" = "true" ]; then
    echo "  ⚠️  CI Mode: Performing ad-hoc signing (no identity)..."
    codesign --force --deep --sign - --entitlements "$ENTITLEMENTS" "$APP_PATH"
    echo "  ✅ Ad-hoc signed"
else
    # Sign the binary explicitly first to ensure entitlements stick to the executable
    echo "  🔐 Signing binary executable (hardened runtime)..."
    codesign --force --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" --options runtime "$BINARY"

    # Then sign the whole bundle
    echo "  🔐 Signing app bundle (hardened runtime)..."
    codesign --force --deep --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" --options runtime "$APP_PATH"
    echo "  ✅ Signed with: $IDENTITY"
fi

echo ""
echo "=== Verification ==="
if [ "$CI" = "true" ]; then
    echo "  ⚠️  CI Mode: Skipping strict verification."
    codesign -dvvv "$APP_PATH" 2>&1 | grep -E "(Identifier=)"
else
    # Verify the signature details
    codesign -dvvv "$APP_PATH" 2>&1 | grep -E "(TeamIdentifier|Authority|Identifier=)" | head -5
    
    echo ""
    echo "=== Verifying Entitlements ==="
    # Check if the binary actually has the sandbox entitlement
    ENTITLEMENTS_DUMP=$(codesign -d --entitlements :- "$BINARY" 2>/dev/null)
    if echo "$ENTITLEMENTS_DUMP" | grep -q "com.apple.security.app-sandbox"; then
        echo "  ✅ Application is Sandboxed"
    else
        echo -e "\033[0;31m  ❌ CRITICAL: Application is NOT Sandboxed (Missing Entitlement)\033[0m"
        echo "     Dumping valid entitlements found:"
        echo "$ENTITLEMENTS_DUMP"
        exit 1
    fi
fi

echo ""
echo "=== Verifying Info.plist ===" 
/usr/libexec/PlistBuddy -c "Print :NSFaceIDUsageDescription" "$PLIST"

echo ""
echo "✅ Release build complete!"
echo "   Run with: $BINARY"
echo "   Or open:  open $APP_PATH"
