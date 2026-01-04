#!/bin/bash
set -e

# Configuration
ENTITLEMENTS_FILE="Hobbes.entitlements"
REQUIRED_ENTITLEMENT="com.apple.security.app-sandbox"
PROVISIONING_PROFILE="embedded.provisionprofile"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "🔍 Checking macOS App Store Readiness..."

# 1. Check Entitlements
if grep -q "$REQUIRED_ENTITLEMENT" "$ENTITLEMENTS_FILE"; then
    echo -e "${GREEN}✅ Entitlements file contains '$REQUIRED_ENTITLEMENT'${NC}"
else
    echo -e "${RED}❌ Missing '$REQUIRED_ENTITLEMENT' in $ENTITLEMENTS_FILE${NC}"
    echo "   The Mac App Store requires the App Sandbox to be enabled."
    exit 1
fi

# 2. Check Provisioning Profile
if [ -f "$PROVISIONING_PROFILE" ]; then
    echo -e "${GREEN}✅ Provisioning profile found: $PROVISIONING_PROFILE${NC}"
else
    echo -e "${RED}❌ Provisioning profile missing: $PROVISIONING_PROFILE${NC}"
    echo "   Please download a Mac App Store Distribution profile from Apple Developer Portal."
    # Don't exit, just warn for now as it might be in a different path for some workflows
fi

# 3. Check Signing Identity
# Use HOBBES_SIGNING_ID if set, otherwise look for "Apple Distribution"
IDENTITY="${HOBBES_SIGNING_ID:-Apple Distribution}"

if security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
    echo -e "${GREEN}✅ Signing identity found matching: '$IDENTITY'${NC}"
else
    echo -e "${YELLOW}⚠️  No signing identity found matching: '$IDENTITY'${NC}"
    echo "   You will need a valid 'Apple Distribution' certificate in your Keychain to sign for the App Store."
    echo "   (This is expected if you are on a machine without distribution certs)"
fi

# 4. Check Installer Signing Identity (NOT codesigning - use find-identity without -p)
if [ -n "$HOBBES_INSTALLER_SIGNING_ID" ]; then
    INSTALLER_IDENTITY="$HOBBES_INSTALLER_SIGNING_ID"
elif security find-identity -v | grep -q "3rd Party Mac Developer Installer"; then
    INSTALLER_IDENTITY=$(security find-identity -v | grep "3rd Party Mac Developer Installer" | head -1 | sed 's/.*"\(.*\)".*/\1/')
elif security find-identity -v | grep -q "Mac Installer Distribution"; then
    INSTALLER_IDENTITY=$(security find-identity -v | grep "Mac Installer Distribution" | head -1 | sed 's/.*"\(.*\)".*/\1/')
else
    INSTALLER_IDENTITY=""
fi

if [ -n "$INSTALLER_IDENTITY" ]; then
    echo -e "${GREEN}✅ Installer signing identity found: '$INSTALLER_IDENTITY'${NC}"
else
    echo -e "${YELLOW}⚠️  No installer signing identity found.${NC}"
    echo "   You will need a '3rd Party Mac Developer Installer' or 'Mac Installer Distribution' certificate to build the .pkg."
fi

echo -e "\n${GREEN}✨ Pre-flight check complete.${NC}"
exit 0
