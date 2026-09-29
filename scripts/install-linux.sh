#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"

cargo build --release -p bridge-viewer

target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
binary="$HOME/.local/bin/airbridge-viewer"
desktop="$HOME/.local/share/applications/airbridge-viewer.desktop"
install -Dm755 "$target_dir/release/bridge-viewer" "$binary"
mkdir -p "$(dirname "$desktop")"
cat > "$desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=AirBridge Viewer
Comment=View a Mac virtual display
Exec="$binary"
Icon=video-display
Terminal=false
Categories=Utility;Video;
StartupWMClass=airbridge-viewer
DESKTOP
chmod 644 "$desktop"

printf 'Installed %s and %s\n' "$binary" "$desktop"
