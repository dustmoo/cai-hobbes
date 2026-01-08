#!/bin/bash
# build_dev.sh - Build for local development/testing
# This script switches to the dev profile and builds with the dev signing identity

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_DIR"

echo "=== Development Build ==="
echo ""

# Switch to dev profile
./scripts/switch_profile.sh dev

# Build with dev identity
export HOBBES_SIGNING_ID="EA4C9CD3EDDE09F48B81734AEB4900D07F67193C"
./scripts/build_release.sh

echo ""
echo "=== Development Build Complete ==="
echo ""
echo "Run the app:"
echo "  ./target/dx/Hobbes/release/macos/Hobbes.app/Contents/MacOS/Hobbes"
echo ""
echo "Or open in Finder:"
echo "  open target/dx/Hobbes/release/macos/Hobbes.app"
echo ""
