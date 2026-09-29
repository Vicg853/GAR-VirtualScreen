#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"

cargo build --release -p bridge-host

target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
bundle="$project_root/dist/AirBridge Host.app"
mkdir -p "$bundle/Contents/MacOS"
cp "$target_dir/release/bridge-host" "$bundle/Contents/MacOS/AirBridgeHost"

cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>AirBridgeHost</string>
    <key>CFBundleIdentifier</key><string>dev.airbridge.host</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>AirBridge Host</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

printf 'Created %s\n' "$bundle"
printf 'Open it from Finder or run: open "%s"\n' "$bundle"
