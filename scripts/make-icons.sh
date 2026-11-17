#!/usr/bin/env bash
# Regenerate the icon files from assets/logo/airbridge.svg. The results are
# committed, so bundling never needs these tools; run this only after editing
# the logo. Needs rsvg-convert (librsvg); the .icns step also needs macOS.
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
logo="$project_root/assets/logo"
command -v rsvg-convert >/dev/null || {
    printf 'rsvg-convert not found. Install librsvg (brew install librsvg / apt install librsvg2-bin).\n' >&2
    exit 1
}

mkdir -p "$logo/png"
for size in 16 32 48 64 128 256 512 1024; do
    rsvg-convert -w "$size" -h "$size" "$logo/airbridge.svg" -o "$logo/png/airbridge-$size.png"
done

if command -v iconutil >/dev/null; then
    iconset="$(mktemp -d)/AirBridge.iconset"
    mkdir -p "$iconset"
    for size in 16 32 128 256 512; do
        cp "$logo/png/airbridge-$size.png" "$iconset/icon_${size}x${size}.png"
        cp "$logo/png/airbridge-$((size * 2)).png" "$iconset/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$iconset" -o "$logo/AirBridge.icns"
    rm -rf "$(dirname "$iconset")"
else
    printf 'iconutil not found: AirBridge.icns was not regenerated (macOS only).\n' >&2
fi
printf 'Icons written to %s\n' "$logo"
