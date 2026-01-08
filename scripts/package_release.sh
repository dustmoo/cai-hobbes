#!/bin/bash
set -e

# Configuration
APP_NAME="Hobbes"
RELEASE_DIR="target/dx/$APP_NAME/release/macos"
APP_BUNDLE="$RELEASE_DIR/$APP_NAME.app"
PKG_OUTPUT="$RELEASE_DIR/$APP_NAME.pkg"
INSTALL_LOC="/Applications"

# Identities
# Code Signing Identity (for the .app) - Used in build_release.sh
# Installer Identity (for the .pkg)
# Note: Installer certs are NOT shown by `-p codesigning`, use `security find-identity -v` without policy filter
if [ -n "$HOBBES_INSTALLER_SIGNING_ID" ]; then
    INSTALLER_IDENTITY="$HOBBES_INSTALLER_SIGNING_ID"
elif security find-identity -v | grep -q "Mac Installer Distribution"; then
    INSTALLER_IDENTITY=$(security find-identity -v | grep "Mac Installer Distribution" | head -1 | sed 's/.*"\(.*\)".*/\1/')
elif security find-identity -v | grep -q "3rd Party Mac Developer Installer"; then
    INSTALLER_IDENTITY=$(security find-identity -v | grep "3rd Party Mac Developer Installer" | head -1 | sed 's/.*"\(.*\)".*/\1/')
else
    echo -e "${RED}❌ No installer signing identity found.${NC}"
    echo "   Please install a 'Mac Installer Distribution' or '3rd Party Mac Developer Installer' certificate."
    exit 1
fi

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

echo "=== Packaging $APP_NAME for App Store ==="

if [ ! -d "$APP_BUNDLE" ]; then
    echo -e "${RED}❌ App bundle not found at $APP_BUNDLE${NC}"
    echo "   Please run ./scripts/build_release.sh first."
    exit 1
fi

echo "📦 Building package..."
echo "   Component: $APP_BUNDLE"
echo "   Install Location: $INSTALL_LOC"
echo "   Signer: $INSTALLER_IDENTITY"

productbuild --component "$APP_BUNDLE" "$INSTALL_LOC" --sign "$INSTALLER_IDENTITY" "$PKG_OUTPUT"

echo ""
echo -e "${GREEN}✅ Package verified and built: $PKG_OUTPUT${NC}"
echo "   Review it:"
echo "   open $RELEASE_DIR"
echo "   Upload this .pkg file to Transporter."
