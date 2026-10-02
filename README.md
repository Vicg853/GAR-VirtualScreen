# AirBridge

AirBridge is a native Mac to Linux extended display experiment. The Mac host creates a virtual display, captures its video, and sends it over an encrypted QUIC connection. The viewer discovers the host on the local network, authenticates with a six digit PIN, decodes the video, and shows the result in a GPUI window. Audio is temporarily disabled while the display path is being tested. If a session requests audio, the host shows a soft warning and continues video-only. Input sharing is outside this project's current scope.

## Build

The macOS host needs Rust, Xcode, and the Xcode Metal Toolchain. The first launch requires macOS Screen Recording permission; restart the host after granting it.

```sh
cargo run -p bridge-host --release
```

The Linux viewer needs Rust and the development libraries required by GPUI for Wayland and X11. The linker needs `libxkbcommon.so` and `libxkbcommon-x11.so` (the development packages, not just the runtime libraries). VA-API bindings are generated with bindgen, which needs both `libclang` and Clang's builtin resource headers (including `stddef.h`). Install matching Clang packages on the **Linux viewer machine**:

```sh
# Fedora
sudo dnf install libxkbcommon-devel libxkbcommon-x11-devel clang clang-devel clang-resource-filesystem

# Ubuntu / Debian
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev clang libclang-dev
```

If `LIBCLANG_PATH` is set to a custom LLVM installation, unset it or point it at the same LLVM version that provides the Clang resource headers. Check that Clang can locate its builtin headers with `clang -print-resource-dir` and that the printed directory contains `include/stddef.h`.

Then run:

```sh
cargo run -p bridge-viewer --release
```

Start the Mac host, then start the viewer. Select the discovered Mac, type the PIN shown in the host window, and click **Connect**. If discovery fails, enter the Mac's IP address in the manual address field. UDP port **44990** and local mDNS traffic must be allowed between the machines.

The connection uses QUIC keepalive packets every five seconds during capture setup. After PIN authentication, control stays available while independent media lanes start. Video uses its own labeled QUIC stream and async sender/receiver tasks. The viewer can disconnect or receive video-lane errors without waiting on capture. Audio and future input/output lanes have reserved labels but are not implemented yet. Both apps must be rebuilt together because this change advances the wire protocol to version 2. A virtual display appearing in macOS Display Settings confirms display creation, but capture must still start before video can arrive.

The host's video-only path uses CoreGraphics display streams, so it does not construct a ScreenCaptureKit stream. ScreenCaptureKit initialized a CoreAudio clock even with audio capture disabled and stalled for about 150 seconds on one Mac. The CoreGraphics display-stream API is marked obsolete by the macOS 15+ SDK and may be removed in a future macOS release; the host reports a visible error if this API is unavailable.

### App launchers

On macOS, run `./scripts/package-macos.sh` to build `dist/AirBridge Host.app`. Open the bundle from Finder and drag it into the Dock if desired. macOS may ask for Screen Recording permission on the first launch; restart the app after granting it.

If macOS still reports that capture was denied, quit all host instances. In **System Settings → Privacy & Security → Screen & System Audio Recording** (called **Screen Recording** on older macOS), remove the stale AirBridge entry, add the exact `AirBridge Host.app` you launch, and enable it. Relaunch from Finder. The startup log shows the actual executable path and the Screen Recording preflight result. Permission given to a terminal-launched binary may not apply to the app bundle.

The packaging script uses ad hoc signing by default, so rebuilding can invalidate saved permission. Build first, then grant access. To sign subsequent builds with a consistent code signing certificate, use `AIRBRIDGE_SIGNING_IDENTITY="Your certificate name" ./scripts/package-macos.sh`. Use the same certificate and bundle identifier for subsequent builds; switching from ad hoc signing requires granting permission to the newly signed app.

On Linux, run `./scripts/install-linux.sh` to build and install the viewer into `~/.local/bin` with an `AirBridge Viewer` application launcher in `~/.local/share/applications`. Launch it from your desktop environment's application menu. The script installs for the current user and does not require root.

The Linux viewer opens two independent windows: a resizable video window and a compact controls widget. The widget can be moved by dragging its header, resized at its edges, minimized, or closed. Use **Controls** in the video window to reopen it. Use **Show display window** in the widget to reopen the video window. Both widgets have an expandable **Logs** section; messages also go to the terminal and to the log file whose path is shown there.

The native video window uses its own wgpu renderer, separate from GPUI, and keeps one compressed packet waiting for the decoder and one decoded frame waiting for presentation. If decoding outruns painting, the older unpainted frame is skipped. The widget reports the skipped-frame count so an overloaded viewer is visible.

## Settings

The host provides Responsive, Balanced, and Detailed presets. The viewer uses the host's selected preset by default. Selecting a preset in the viewer overrides the host's selection for that connection. The codec submenu exposes orientation, frame rate, bitrate, keyframe interval, H.264 profile, and low latency rate control. Changes to display settings take effect on the next connection. **Save custom preset** writes to the local `airbridge/presets.json` configuration file on that machine.

The current session sends video packets with sequence numbers and timestamps; the protocol retains an audio packet type for later use. The host currently sends H.264. On Linux, the viewer probes hardware decoders in this order: NVIDIA NVDEC, VA-API (commonly Intel and AMD), then V4L2 (embedded SoCs). If none can open the stream, it uses OpenH264 software decoding. Hardware decoding and display rendering are separate choices: Vulkan DMA-BUF import can keep supported VA-API frames on the GPU, while unsupported imports and NVDEC CUDA frames currently use the decoder's CPU readback path before GPU presentation.

## Current limits

- The virtual display uses macOS's private `CGVirtualDisplay` API. It can change between macOS releases and does not suit Mac App Store distribution.
- Only one viewer and one virtual display are active at a time.
- Linux H.264 decode uses NVDEC, VA-API, or V4L2 when available, with OpenH264 software fallback. AV1 is not yet negotiated by the host/viewer stream, even though NVDEC support exists in the backend dependency.
- Linux VA-API DMA-BUF frames can be imported into Vulkan for GPU rendering when the decoder and presentation GPU/driver support sharing the exported surface. NVIDIA NVDEC is hardware accelerated, but its CUDA output currently requires a CPU readback before wgpu presentation.
- The current media path uses an ordered QUIC stream. Packet loss can delay later frames. Independent video frame streams or datagrams are a future latency improvement.
- The common codec ranking still describes future negotiation preference; this session currently streams H.264 only.
- The VSync setting is stored with presets; explicit display presentation control is not wired to it yet.
- The Linux build and a two machine streaming session still need validation on Fedora hardware. The viewer has been compiled and launched on macOS to check the shared Rust and GPUI code paths.

The host's TLS certificate and private key are kept under the local `airbridge` configuration directory. The private key is mode `0600` on Unix. PIN authentication uses SPAKE2 and confirms the result against the QUIC TLS exporter before starting the media stream.
