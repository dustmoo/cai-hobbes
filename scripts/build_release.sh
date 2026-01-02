#!/bin/bash
# Build, patch, and sign the Hobbes release binary for macOS
# This script ensures proper Info.plist configuration and code signing for biometric keychain access.
set -e

APP_PATH="target/dx/Hobbes/release/macos/Hobbes.app"
BINARY="$APP_PATH/Contents/MacOS/Hobbes"
PLIST="$APP_PATH/Contents/Info.plist"
ENTITLEMENTS="Hobbes.entitlements"
IDENTITY="${HOBBES_SIGNING_ID:-Apple Development: dustin@tulipvalleytech.com (4753E57CRM)}"

echo "=== Building Release ==="
dx build --release

echo ""
echo "=== Installing Icon (Dioxus 0.6 workaround) ==="
./scripts/install_icon.sh

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

echo ""
echo "=== Embedding Provisioning Profile ==="
PROVISIONING_PROFILE="./embedded.provisionprofile"
if [ "$CI" = "true" ]; then
    echo "  ⚠️  CI Mode: Skipping Provisioning Profile embedding."
elif [ -f "$PROVISIONING_PROFILE" ]; then
    cp "$PROVISIONING_PROFILE" "$APP_PATH/Contents/embedded.provisionprofile"
    echo "  ✅ Embedded provisioning profile"
else
    echo "  ❌ Error: Provisioning profile not found at $PROVISIONING_PROFILE"
    echo "     Please ensure 'embedded.provisionprofile' is in the project root."
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
    echo "  🔐 Signing with Developer Certificate..."
    codesign --force --deep --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" "$APP_PATH"
    echo "  ✅ Signed with: $IDENTITY"
fi

echo ""
echo "=== Verification ==="
if [ "$CI" = "true" ]; then
    echo "  ⚠️  CI Mode: Skipping strict verification."
    codesign -dvvv "$APP_PATH" 2>&1 | grep -E "(Identifier=)"
else
    codesign -dvvv "$APP_PATH" 2>&1 | grep -E "(TeamIdentifier|Authority|Identifier=)" | head -5
fi

echo ""
echo "=== Verifying Info.plist ===" 
/usr/libexec/PlistBuddy -c "Print :NSFaceIDUsageDescription" "$PLIST"

echo ""
echo "✅ Release build complete!"
echo "   Run with: $BINARY"
echo "   Or open:  open $APP_PATH"
