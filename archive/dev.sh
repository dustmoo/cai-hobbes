#!/bin/bash
# dev.sh - Dioxus dev server with automatic code signing for biometric keychain

set -e

BINARY="target/debug/Hobbes"
ENTITLEMENTS="Hobbes.entitlements"
IDENTITY="-" # Ad-hoc signing

echo "🔐 Automatic Signing enabled (Identity: $IDENTITY)"
echo "📁 Watching: $BINARY"

# Function to sign
sign_binary() {
    if [ -f "$BINARY" ]; then
        echo "📝 Signing binary..."
        codesign --force --deep --sign "$IDENTITY" --entitlements "$ENTITLEMENTS" "$BINARY" 2>/dev/null
        echo "✅ Signed $BINARY at $(date '+%H:%M:%S')"
        # Mark as signed
        stat -f %m "$BINARY" > /tmp/hobbes_last_signed_mtime 2>/dev/null
    fi
}

cleanup() {
    echo "🛑 Shutting down..."
    kill $WATCHER_PID 2>/dev/null || true
    rm -f /tmp/hobbes_last_signed_mtime
    exit 0
}
trap cleanup SIGINT SIGTERM EXIT

# Initialize
echo "0" > /tmp/hobbes_last_signed_mtime

# Watcher Loop
(
    while true; do
        sleep 1
        if [ -f "$BINARY" ]; then
            current_mtime=$(stat -f %m "$BINARY" 2>/dev/null || echo "0")
            last_signed=$(cat /tmp/hobbes_last_signed_mtime 2>/dev/null || echo "0")
            
            # If changed
            if [ "$current_mtime" != "$last_signed" ]; then
                # Wait for file to stabilize (build finish)
                sleep 0.5
                sign_binary
            fi
        fi
    done
) &
WATCHER_PID=$!

echo "🚀 Starting dx serve..."
dx serve
