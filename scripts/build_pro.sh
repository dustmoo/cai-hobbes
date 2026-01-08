#!/bin/bash
# Build, patch, and sign the Hobbes PRO (Direct Download) binary for macOS
# This script ensures proper entitlements for non-sandboxed execution (Tool usage).
set -e

# PRO builds go to a separate directory to avoid overwriting App Store builds
PRO_OUTPUT_DIR="target/dx/Hobbes/release/macos/pro"
APP_PATH="$PRO_OUTPUT_DIR/Hobbes.app"
BINARY="$APP_PATH/Contents/MacOS/Hobbes"
PLIST="$APP_PATH/Contents/Info.plist"
ENTITLEMENTS="HobbesPro.entitlements"
# Default to "Developer ID Application" if not set, fallback to Development for testing
IDENTITY="${HOBBES_PRO_SIGNING_ID:-Developer ID Application}"

# If HOBBES_PRO_SIGNING_ID isn't manually set, try to find a valid Developer ID
# If HOBBES_PRO_SIGNING_ID isn't manually set, try to find a valid Developer ID
if [ -z "$HOBBES_PRO_SIGNING_ID" ]; then
    if security find-identity -v -p codesigning | grep -q "Developer ID Application"; then
        IDENTITY="Developer ID Application"
        echo "✅ Detected 'Developer ID Application' identity"
    else
        echo "⚠️  No 'Developer ID Application' certificate found."
        echo "   Falling back to 'Apple Development' for local testing."
        IDENTITY="Apple Development"
    fi
fi

# Ensure 12.0+ for arm64 sanity
export MACOSX_DEPLOYMENT_TARGET=12.0

echo "=== Building PRO Release Bundle (Direct Download) ==="
dx bundle --release

# Copy the built app to PRO directory (dx outputs to default location)
DX_OUTPUT="target/dx/Hobbes/release/macos/Hobbes.app"
echo ""
echo "=== Copying to PRO output directory ==="
mkdir -p "$PRO_OUTPUT_DIR"
rm -rf "$APP_PATH"
cp -R "$DX_OUTPUT" "$APP_PATH"
echo "  ✅ Copied to $APP_PATH"

# PRO builds must NOT have an embedded provisioning profile (no sandbox, no keychain access groups)
# Remove it if present to avoid confusion and signing issues
if [ -f "$APP_PATH/Contents/embedded.provisionprofile" ]; then
    rm "$APP_PATH/Contents/embedded.provisionprofile"
    echo "  🗑️  Removed embedded.provisionprofile (not needed for PRO/Developer ID)"
fi

# Fail-safe: Ensure binary exists
if [ ! -f "$BINARY" ]; then
    echo "⚠️  Binary missing in bundle! Attempting manual copy..."
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
echo "=== Installing Icon (PRO) ==="
ICON_SOURCE="assets/icon-pro.icns"
if [ -f "$ICON_SOURCE" ]; then
    cp "$ICON_SOURCE" "$APP_PATH/Contents/Resources/icon.icns"
    echo "  ✅ Copied PRO icon to bundle"
else
    echo "  ⚠️  PRO icon missing at $ICON_SOURCE"
fi

# Patch version in plist (Dioxus 0.6 workaround)
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
if [ -f "$PLIST" ]; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $VERSION" "$PLIST"
    
    BUILD_VERSION=$(echo "$VERSION" | sed 's/-.*//') 
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_VERSION" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $BUILD_VERSION" "$PLIST"
    
    # Set LSMinimumSystemVersion to match MACOSX_DEPLOYMENT_TARGET
    /usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion 12.0" "$PLIST" 2>/dev/null || \
    /usr/libexec/PlistBuddy -c "Add :LSMinimumSystemVersion string 12.0" "$PLIST"
    
    echo "  ✅ Patched version to $VERSION"
fi

# Wait for filesystem stability
sleep 2

echo "=== Patching Info.plist ==="
# Add specific description for why we need camera/mic if we requested them in entitlements
# (Already handled for FaceID, but let's Ensure)
if ! /usr/libexec/PlistBuddy -c "Print :NSFaceIDUsageDescription" "$PLIST" 2>/dev/null; then
    /usr/libexec/PlistBuddy -c "Add :NSFaceIDUsageDescription string 'Hobbes uses Touch ID to securely access your API keys in the Keychain.'" "$PLIST"
fi

# Set the app name to "Hobbes Pro" for direct download builds
/usr/libexec/PlistBuddy -c "Set :CFBundleName Hobbes Pro" "$PLIST" 2>/dev/null || \
/usr/libexec/PlistBuddy -c "Add :CFBundleName string Hobbes Pro" "$PLIST"

# Add attribution/copyright
/usr/libexec/PlistBuddy -c "Set :NSHumanReadableCopyright Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families." "$PLIST" 2>/dev/null || \
/usr/libexec/PlistBuddy -c "Add :NSHumanReadableCopyright string Made w/ ❤️ by Clear Mirror LLC, Gemini 2.5, 3 and Claude model families." "$PLIST"

echo "  ✅ Patched Info.plist (PRO branding + attribution)"

echo ""
echo "=== Cleaning extended attributes ==="
xattr -cr "$APP_PATH"

echo ""
echo "=== Code Signing (PRO / No Sandbox) ==="
echo "  Entitlements: $ENTITLEMENTS"
echo "  Identity:     $IDENTITY"

# Note: For Developer ID distribution, we typically need a timestamped signature
TIMESTAMP_FLAG="--timestamp"
if [[ "$IDENTITY" == *"Apple Development"* ]]; then
    TIMESTAMP_FLAG="--timestamp=none" # Local dev often doesn't need strict timestamping or it might fail if offline
fi

codesign --force --deep --options runtime $TIMESTAMP_FLAG --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" "$APP_PATH"
echo "  ✅ Signed with: $IDENTITY"

echo ""
echo "=== Verification ==="
codesign -dvvv --entitlements - "$APP_PATH" 2>&1 | grep -E "(Identifier=|Authority|com.apple.security.app-sandbox)" || true

echo ""
echo "✅ PRO Build Complete!"
echo "   Location: $APP_PATH"
echo "   NOTE: To distribute this to others, you must NOTARIZE it with Apple."
