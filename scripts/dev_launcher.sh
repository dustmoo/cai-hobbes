#!/bin/bash
# scripts/dev_launcher.sh
#
# A development launcher that mimics the release build environment (codesigning, entitlements)
# but uses 'cargo build' instead of 'dx' to ensure permissions (TCC/Bio) work correctly.
#
# Features:
# - Starts Tailwind watcher in background
# - Builds debug binary
# - PACKAGES into a valid macOS .app bundle (Critical for Provisioning Profiles)
# - Codesigns with entitlements 
# - Runs the app bundle executable

set -e

# Configuration
BINARY_NAME="Hobbes"
BUILD_DIR="target/debug"
# Staging area for constructing the .app
APP_STAGING_DIR="$BUILD_DIR/mac_app/${BINARY_NAME}.app"
CONTENTS_DIR="$APP_STAGING_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

ENTITLEMENTS="Hobbes.dev.entitlements"
PROVISIONING_PROFILE="embedded.provisionprofile"
IDENTITY="${HOBBES_SIGNING_ID:-Developer ID Application: DUSTIN ALAN MOORE (ABXVW6PWCW)}"

TAILWIND_INPUT="tailwind.css"
TAILWIND_OUTPUT="assets/tailwind.css"

echo "=== 🚀 Hobbes Dev Launcher ==="

# 1. Start Tailwind Watcher
echo "🎨 Starting Tailwind Watcher..."
npx tailwindcss -i "./$TAILWIND_INPUT" -o "./$TAILWIND_OUTPUT" --watch > /dev/null 2>&1 &
TAILWIND_PID=$!
echo "   Tailwind PID: $TAILWIND_PID"

# Function to cleanup background process on exit
cleanup() {
    echo ""
    echo "🛑 Stopping Tailwind Watcher..."
    kill $TAILWIND_PID
    exit
}
trap cleanup SIGINT SIGTERM

# 2. Build & Run Loop
while true; do
    echo ""
    echo "=========================================="
    echo "🔨 Building (Debug)..."
    echo "=========================================="
    
    # Capture build status
    if cargo build; then
        echo ""
        echo "📦 Packaging .app Bundle (required for Profiles)..."
        
        # Cleanup and recreate structure
        rm -rf "$APP_STAGING_DIR"
        mkdir -p "$MACOS_DIR"
        mkdir -p "$RESOURCES_DIR"
        
        # Copy Binary
        cp "$BUILD_DIR/hobbes" "$MACOS_DIR/$BINARY_NAME"
        chmod +x "$MACOS_DIR/$BINARY_NAME"
        
        # Copy Resources (assets/etc)
        # Note: Dioxus expects assets to be relative to the executable or in Resources?
        # Standard .app: Resources/assets or just Resources/
        # Dioxus 0.6 usually looks in .app/Contents/Resources/assets if bundled.
        cp -r "assets" "$RESOURCES_DIR/" 
        
        # Generate Info.plist (Minimal required for execution + Bio)
        cat > "$CONTENTS_DIR/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key>
    <string>Hobbes Dev</string>
    <key>CFBundleExecutable</key>
    <string>${BINARY_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>ai.clearmirror.cai-hobbes</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Hobbes</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.9.4-dev</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>NSFaceIDUsageDescription</key>
    <string>Hobbes uses Touch ID to securely access your API keys in the Keychain.</string>
</dict>
</plist>
EOF

        # Personal signing fix: Skip entitlements if using personal cert to avoid Killed: 9
        if [[ "$IDENTITY" == *"4753E57CRM"* ]]; then
             echo "⚠️  Personal Team ID detected. Skipping Entitlements/Provisioning to prevent crash."
             ENTITLEMENTS=""
             PROVISIONING_PROFILE=""
        fi

        # Embed Provisioning Profile (Critical for entitlements)
        if [ -f "$PROVISIONING_PROFILE" ]; then
            cp "$PROVISIONING_PROFILE" "$CONTENTS_DIR/embedded.provisionprofile"
            echo "   ✅ Embedded Provisioning Profile"
        else
            echo "   ⚠️  WARNING: Provisioning Profile not found at $PROVISIONING_PROFILE"
        fi

        echo "🧹 Cleaning extended attributes..."
        xattr -cr "$APP_STAGING_DIR"

        echo "🔐 Signing App Bundle..."
        if [ -f "$ENTITLEMENTS" ]; then
            codesign --force --deep --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" "$APP_STAGING_DIR"
            echo "   ✅ Signed with entitlements"
        else
            echo "   ⚠️  WARNING: Entitlements file not found at $ENTITLEMENTS"
            codesign --force --deep --sign "$IDENTITY" "$APP_STAGING_DIR"
        fi

        # Notarization (Required for Developer ID certs to pass Gatekeeper)
        # Skip automatically for Apple Development certs
        if [[ "$IDENTITY" == *"Apple Development"* ]]; then
             echo "🛡️  Skipping notarization (Apple Development certificate detected)"
             SKIP_NOTARIZE="true"
        fi

        if [ "$SKIP_NOTARIZE" != "true" ]; then
            echo ""
            echo "🛡️  Notarizing (Required for Developer ID)..."
            NOTARY_PROFILE="${NOTARY_PROFILE:-AC_PASSWORD_PROFILE}"
            ZIP_PATH="$BUILD_DIR/Hobbes-Dev.zip"
            
            # Zip for upload
            rm -f "$ZIP_PATH"
            ditto -c -k --keepParent "$APP_STAGING_DIR" "$ZIP_PATH"
            
            echo "   Submitting to Apple (Profile: $NOTARY_PROFILE)..."
            echo "   (This takes 1-3 minutes...)"
            
            if xcrun notarytool submit "$ZIP_PATH" --keychain-profile "$NOTARY_PROFILE" --wait; then
                echo "   ✅ Notarized successfully"
                echo "   Stapling ticket..."
                xcrun stapler staple "$APP_STAGING_DIR"
                echo "   ✅ Stapled"
            else
                echo "   ❌ Notarization failed. Launch may fail due to Gatekeeper." 
                echo "   Set SKIP_NOTARIZE=true to skip this step."
                # We don't exit here, we let it try to run anyway
            fi
        fi
        
        echo ""
        echo "▶️  Running Hobbes..."
        echo "   (Press Ctrl+C to stop script, or quit app to rebuild)"
        echo "=========================================="
        
        # Run the executable inside the bundle
        "$MACOS_DIR/$BINARY_NAME"
        
        echo ""
        echo "✅ App exited."
    else
        echo ""
        echo "❌ Build Failed."
    fi
    
    echo ""
    read -p "🔄 Press Enter to Rebuild & Restart..."
done
