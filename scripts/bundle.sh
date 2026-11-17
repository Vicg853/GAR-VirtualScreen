#!/usr/bin/env bash
# Build AirBridge for this computer and bundle it as a launchable app with its icon.
#
#   macOS  → dist/AirBridge Host.app            (the host runs on macOS)
#   Linux  → dist/airbridge-viewer-linux-<arch>/ (the viewer runs on Linux)
#
# Options:
#   --install   also install it: /Applications (or ~/Applications) on macOS,
#               ~/.local on Linux, with an application-menu launcher
#   --archive   also write a .zip (macOS) or .tar.gz (Linux) next to the bundle
#   --no-build  bundle the release binary that is already built
set -euo pipefail

install=false
archive=false
build=true
for argument in "$@"; do
    case "$argument" in
        --install) install=true ;;
        --archive) archive=true ;;
        --no-build) build=false ;;
        -h | --help)
            sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            printf 'Unknown option: %s (try --help)\n' "$argument" >&2
            exit 2
            ;;
    esac
done

project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
logo="$project_root/assets/logo"
dist="$project_root/dist"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/bridge-core/Cargo.toml | head -1)"

bundle_macos() {
    $build && cargo build --release -p bridge-host
    local bundle="$dist/AirBridge Host.app"
    rm -rf "$bundle"
    mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
    cp "$target_dir/release/bridge-host" "$bundle/Contents/MacOS/AirBridgeHost"
    cp "$logo/AirBridge.icns" "$bundle/Contents/Resources/AirBridge.icns"

    cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>AirBridgeHost</string>
    <key>CFBundleIconFile</key><string>AirBridge</string>
    <key>CFBundleIdentifier</key><string>dev.airbridge.host</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>AirBridge Host</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

    local signing_identity="${AIRBRIDGE_SIGNING_IDENTITY:--}"
    codesign --force --sign "$signing_identity" --identifier dev.airbridge.host "$bundle"
    printf 'Created %s\n' "$bundle"

    if $archive; then
        local zip="$dist/AirBridge-Host-$version-macos-$(uname -m).zip"
        rm -f "$zip"
        ditto -c -k --keepParent "$bundle" "$zip"
        printf 'Created %s\n' "$zip"
    fi

    local launch="$bundle"
    if $install; then
        local applications=/Applications
        [[ -w "$applications" ]] || applications="$HOME/Applications"
        mkdir -p "$applications"
        rm -rf "$applications/AirBridge Host.app"
        # ditto keeps the signature intact.
        ditto "$bundle" "$applications/AirBridge Host.app"
        launch="$applications/AirBridge Host.app"
        printf 'Installed %s\n' "$launch"
    fi

    if [[ "$signing_identity" == "-" ]]; then
        printf 'Ad hoc signing: rebuilding can invalidate Screen Recording permission. Grant access after the final build.\n'
        printf 'For a stable signing identity, set AIRBRIDGE_SIGNING_IDENTITY to your code signing certificate name.\n'
    fi
    printf 'Open it from Finder or run: open "%s"\n' "$launch"
}

bundle_linux() {
    $build && cargo build --release -p bridge-viewer
    local name="airbridge-viewer-linux-$(uname -m)"
    local bundle="$dist/$name"
    rm -rf "$bundle"
    mkdir -p "$bundle/icons"
    install -m755 "$target_dir/release/bridge-viewer" "$bundle/airbridge-viewer"
    cp "$logo/airbridge.svg" "$bundle/icons/airbridge-viewer.svg"
    for size in 16 32 48 64 128 256 512; do
        cp "$logo/png/airbridge-$size.png" "$bundle/icons/airbridge-viewer-$size.png"
    done

    # The bundle installs itself, so it can be copied to another Linux computer.
    cat > "$bundle/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install AirBridge Viewer for the current user (no root needed).
# Set PREFIX to install elsewhere, e.g. PREFIX=/usr/local sudo -E ./install.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
binary="$prefix/bin/airbridge-viewer"
desktop="$prefix/share/applications/airbridge-viewer.desktop"

install -Dm755 "$here/airbridge-viewer" "$binary"
install -Dm644 "$here/icons/airbridge-viewer.svg" \
    "$prefix/share/icons/hicolor/scalable/apps/airbridge-viewer.svg"
for size in 16 32 48 64 128 256 512; do
    install -Dm644 "$here/icons/airbridge-viewer-$size.png" \
        "$prefix/share/icons/hicolor/${size}x${size}/apps/airbridge-viewer.png"
done

mkdir -p "$(dirname "$desktop")"
cat > "$desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=AirBridge Viewer
Comment=View a Mac virtual display
Exec="$binary"
Icon=airbridge-viewer
Terminal=false
Categories=Utility;Video;
StartupWMClass=airbridge-viewer
DESKTOP
chmod 644 "$desktop"

# Optional caches; the launcher still works without them after a re-login.
command -v update-desktop-database >/dev/null && update-desktop-database -q "$prefix/share/applications" || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f "$prefix/share/icons/hicolor" || true

printf 'Installed %s\nLaunch “AirBridge Viewer” from the application menu.\n' "$binary"
INSTALL
    cat > "$bundle/uninstall.sh" <<'UNINSTALL'
#!/usr/bin/env bash
# Remove what install.sh installed. Saved sessions in ~/.config/airbridge are kept.
set -euo pipefail
prefix="${PREFIX:-$HOME/.local}"
rm -f "$prefix/bin/airbridge-viewer" \
    "$prefix/share/applications/airbridge-viewer.desktop" \
    "$prefix/share/icons/hicolor/scalable/apps/airbridge-viewer.svg"
for size in 16 32 48 64 128 256 512; do
    rm -f "$prefix/share/icons/hicolor/${size}x${size}/apps/airbridge-viewer.png"
done
printf 'AirBridge Viewer removed from %s\n' "$prefix"
UNINSTALL
    chmod 755 "$bundle/install.sh" "$bundle/uninstall.sh"
    printf 'Created %s\n' "$bundle"

    if $archive; then
        local tarball="$dist/airbridge-viewer-$version-linux-$(uname -m).tar.gz"
        tar -czf "$tarball" -C "$dist" "$name"
        printf 'Created %s\n' "$tarball"
    fi

    if $install; then
        "$bundle/install.sh"
    else
        printf 'Install it with: "%s/install.sh"\n' "$bundle"
    fi
}

case "$(uname -s)" in
    Darwin) bundle_macos ;;
    Linux) bundle_linux ;;
    *)
        printf 'Bundling is supported on macOS (host) and Linux (viewer).\n' >&2
        exit 1
        ;;
esac
