#!/bin/bash
# Post-build script to copy icon and patch version in Dioxus app bundle
# Workaround for Dioxus 0.6 icon bundling bug
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Source icon
ICON_SOURCE="$PROJECT_ROOT/assets/icon.icns"

# Extract version from Cargo.toml
VERSION=$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "Project version: $VERSION"

# Debug bundle paths
DEBUG_BUNDLE="$PROJECT_ROOT/target/dx/Hobbes/debug/macos/Hobbes.app/Contents/Resources"
DEBUG_PLIST="$PROJECT_ROOT/target/dx/Hobbes/debug/macos/Hobbes.app/Contents/Info.plist"

# Release bundle paths
RELEASE_BUNDLE="$PROJECT_ROOT/target/dx/Hobbes/release/macos/Hobbes.app/Contents/Resources"
RELEASE_PLIST="$PROJECT_ROOT/target/dx/Hobbes/release/macos/Hobbes.app/Contents/Info.plist"

# Function to patch a bundle
patch_bundle() {
    local BUNDLE_DIR="$1"
    local PLIST="$2"
    local TYPE="$3"
    
    if [ ! -d "$BUNDLE_DIR" ]; then
        return
    fi
    
    # Copy icon
    if [ -f "$ICON_SOURCE" ]; then
        cp "$ICON_SOURCE" "$BUNDLE_DIR/icon.icns"
        echo "✓ Copied icon to $TYPE bundle"
    fi
    
    # Patch version in plist
    if [ -f "$PLIST" ]; then
        /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$PLIST" 2>/dev/null || \
        /usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $VERSION" "$PLIST"
        
        # For CFBundleVersion, use just the numeric part (no -rc suffix)
        BUILD_VERSION=$(echo "$VERSION" | sed 's/-.*//') 
        /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD_VERSION" "$PLIST" 2>/dev/null || \
        /usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $BUILD_VERSION" "$PLIST"
        
        echo "✓ Patched $TYPE version to $VERSION"
    fi
}

# Patch debug bundle
patch_bundle "$DEBUG_BUNDLE" "$DEBUG_PLIST" "debug"

# Patch release bundle
patch_bundle "$RELEASE_BUNDLE" "$RELEASE_PLIST" "release"

# Touch the app bundles to refresh
if [ -d "$PROJECT_ROOT/target/dx/Hobbes/debug/macos/Hobbes.app" ]; then
    touch "$PROJECT_ROOT/target/dx/Hobbes/debug/macos/Hobbes.app"
fi
if [ -d "$PROJECT_ROOT/target/dx/Hobbes/release/macos/Hobbes.app" ]; then
    touch "$PROJECT_ROOT/target/dx/Hobbes/release/macos/Hobbes.app"
fi

echo "✓ Bundle patching complete"
