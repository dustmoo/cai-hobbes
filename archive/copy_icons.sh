#!/bin/bash
set -e
mkdir -p assets/icons/hobbes.iconset
SRC="temp_raw_icons"
DST="assets/icons/hobbes.iconset"

cp "$SRC/Icon-macOS-16x16@1x.png" "$DST/icon_16x16.png"
cp "$SRC/Icon-macOS-16x16@2x.png" "$DST/icon_16x16@2x.png"
cp "$SRC/Icon-macOS-32x32@1x.png" "$DST/icon_32x32.png"
cp "$SRC/Icon-macOS-32x32@2x.png" "$DST/icon_32x32@2x.png"
cp "$SRC/Icon-macOS-128x128@1x.png" "$DST/icon_128x128.png"
cp "$SRC/Icon-macOS-128x128@2x.png" "$DST/icon_128x128@2x.png"
cp "$SRC/Icon-macOS-256x256@1x.png" "$DST/icon_256x256.png"
cp "$SRC/Icon-macOS-256x256@2x.png" "$DST/icon_256x256@2x.png"
cp "$SRC/Icon-macOS-512x512@1x.png" "$DST/icon_512x512.png"
cp "$SRC/Icon-macOS-512x512@2x.png" "$DST/icon_512x512@2x.png"

echo "Copy complete."
ls -la "$DST"
