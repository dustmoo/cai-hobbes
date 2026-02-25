#!/bin/bash
# Generate Windows .ico files from Mac icon PNGs
# Requires: brew install imagemagick
#
# Usage: ./scripts/generate_windows_icon.sh
# Produces: assets/icon.ico and assets/icon-pro.ico

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Standard icon from archive/temp_raw_icons
SRC_STD="$PROJECT_DIR/archive/temp_raw_icons/Icon-macOS-512x512@2x.png"
OUT_STD="$PROJECT_DIR/assets/icon.ico"

# Pro icon from tmp_pro_icons
SRC_PRO="$PROJECT_DIR/tmp_pro_icons/Icon-macOS-512x512@2x.png"
OUT_PRO="$PROJECT_DIR/assets/icon-pro.ico"

generate_ico() {
    local src="$1"
    local out="$2"
    local label="$3"

    if [ ! -f "$src" ]; then
        echo "  [SKIP] Source not found: $src"
        return
    fi

    magick "$src" \
        \( -clone 0 -resize 16x16 \) \
        \( -clone 0 -resize 32x32 \) \
        \( -clone 0 -resize 48x48 \) \
        \( -clone 0 -resize 64x64 \) \
        \( -clone 0 -resize 128x128 \) \
        \( -clone 0 -resize 256x256 \) \
        -delete 0 "$out"

    echo "  [OK] $label: $out ($(du -h "$out" | cut -f1) )"
}

echo "=== Generating Windows Icons ==="
generate_ico "$SRC_STD" "$OUT_STD" "Standard"
generate_ico "$SRC_PRO" "$OUT_PRO" "Pro"
echo "=== Done ==="
