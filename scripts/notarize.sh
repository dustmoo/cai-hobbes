#!/bin/bash
# Automate Apple Notarization for the Pro (Direct Download) Release
# Prerequisite: You must store your credentials first via:
# xcrun notarytool store-credentials "AC_PASSWORD_PROFILE" --apple-id "your@email.com" --team-id "ABXVW6PWCW" --password "app-specific-password"

set -e

APP_PATH="target/dx/Hobbes/release/macos/pro/Hobbes.app"
ZIP_PATH="target/dx/Hobbes/release/macos/pro/Hobbes-Pro.zip"
KEYCHAIN_PROFILE="${NOTARY_PROFILE:-AC_PASSWORD_PROFILE}"

# Check if app exists
if [ ! -d "$APP_PATH" ]; then
    echo "❌ Error: App not found at $APP_PATH"
    echo "   Run ./scripts/build_pro.sh first."
    exit 1
fi

echo "=== Packaging for Notarization ==="
# Remove old zip if exists
rm -f "$ZIP_PATH"
# Zip the app (ditto preserves resource forks and permissions strictly)
ditto -c -k --keepParent "$APP_PATH" "$ZIP_PATH"
echo "✅ Created $ZIP_PATH"

echo ""
echo "=== Submitting to Apple Notarization Service ==="
echo "   Profile: $KEYCHAIN_PROFILE"
echo "   This may take several minutes..."

# Submit and wait
xcrun notarytool submit "$ZIP_PATH" --keychain-profile "$KEYCHAIN_PROFILE" --wait

echo ""
echo "=== Stapling the Ticket ==="
# Staple the ticket to the original .app (so it works offline)
xcrun stapler staple "$APP_PATH"

echo ""
echo "=== Verification ==="
spctl --assess --verbose "$APP_PATH"

echo ""
echo "✅ Notarization Complete!"
echo "   The app at $APP_PATH is now ready for distribution."
echo "   You can now zip it again or put it in a DMG."
