# AirBridge

AirBridge is a native Mac to Linux extended display experiment. The Mac host creates a virtual display, captures its video, and sends it over an encrypted QUIC connection. The viewer discovers the host on the local network, authenticates with a six digit PIN, decodes the video, and shows each stream in its own native video window. Audio is temporarily disabled while the display path is being tested. If a session requests audio, the host shows a soft warning and continues video-only. Input sharing is outside this project's current scope.

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

Start the Mac host, then start the viewer. On the host, add/select a screen and open its details to see its PIN. On the viewer, add a local screen, select the discovered host screen inside its details, enter that screen's PIN, and click **Connect**. If discovery fails, enter the Mac's IP address in the manual address field. UDP port **44990** and local mDNS traffic must be allowed between the machines.

The connection uses QUIC keepalive packets every five seconds during capture setup. After PIN authentication, a dedicated bidirectional control stream stays separate from media. Each screen and each media lane uses its own labeled QUIC unidirectional stream with independent async sender/receiver tasks. A stalled or restarted screen lane therefore does not block another screen's media lane. Audio and future input/output lanes have reserved labels but are not implemented yet. Both apps must be rebuilt together because the wire protocol is version 5. A virtual display appearing in macOS Display Settings confirms display creation, but capture must still start before video can arrive.

The host's video-only path uses CoreGraphics display streams, so it does not construct a ScreenCaptureKit stream. ScreenCaptureKit initialized a CoreAudio clock even with audio capture disabled and stalled for about 150 seconds on one Mac. The CoreGraphics display-stream API is marked obsolete by the macOS 15+ SDK and may be removed in a future macOS release; the host reports a visible error if this API is unavailable.

### App launchers

On macOS, run `./scripts/package-macos.sh` to build `dist/AirBridge Host.app`. Open the bundle from Finder and drag it into the Dock if desired. macOS may ask for Screen Recording permission on the first launch; restart the app after granting it.

If macOS still reports that capture was denied, quit all host instances. In **System Settings → Privacy & Security → Screen & System Audio Recording** (called **Screen Recording** on older macOS), remove the stale AirBridge entry, add the exact `AirBridge Host.app` you launch, and enable it. Relaunch from Finder. The startup log shows the actual executable path and the Screen Recording preflight result. Permission given to a terminal-launched binary may not apply to the app bundle.

The packaging script uses ad hoc signing by default, so rebuilding can invalidate saved permission. Build first, then grant access. To sign subsequent builds with a consistent code signing certificate, use `AIRBRIDGE_SIGNING_IDENTITY="Your certificate name" ./scripts/package-macos.sh`. Use the same certificate and bundle identifier for subsequent builds; switching from ad hoc signing requires granting permission to the newly signed app.

On Linux, run `./scripts/install-linux.sh` to build and install the viewer into `~/.local/bin` with an `AirBridge Viewer` application launcher in `~/.local/share/applications`. Launch it from your desktop environment's application menu. The script installs for the current user and does not require root.

The Linux viewer opens a compact GPUI controls widget and an independent native video window for each screen. Select a screen row to open its details; **← Stream list** returns to the list while preserving the selection highlight. **Add stream** is available in the connected list. Settings, presets, statistics, logs, **Show display window**, **Restart/Retry stream**, and **Remove stream** belong to that screen's details. Before connecting, host selection, address/PIN entry, and next-connection settings are shown; while connecting, **Cancel connection** is shown; during a session, **Disconnect all streams** is shown.

Closing a native video window removes that screen on the host. Removing a screen on the host closes its viewer window. A video lane failure closes its display and keeps its control row available for retry or removal. The primary screen can be removed too; an empty screen list remains connected and can add screens again. Closing the controls widget quits the app and tears down the whole session. Widgets can be dragged by the header, resized at the edges, and minimized. **Show display window** brings an existing live window to the front. Both widgets have expandable **Logs** sections; messages also go to the terminal and the displayed log file.

The native video window uses its own wgpu renderer, separate from GPUI, and keeps one compressed packet waiting for the decoder and one decoded frame waiting for presentation. If decoding outruns painting, the older unpainted frame is skipped. The widget reports the skipped-frame count so an overloaded viewer is visible.

## Wire protocol v5

Each viewer-local screen opens an independent QUIC session to exactly one existing host screen. Before PIN proof, a bounded version/target selector chooses the screen. SPAKE2 identities bind the selected screen and protocol version, and proof confirmation binds the TLS exporter. The host replies with its runtime host identity, subscriber identity, and complete screen configuration. Screen creation/removal belongs to the host widget. Request IDs, settings revisions and capture generations scope later messages.

Each host screen owns one prepared virtual display and one capture/encoder source shared by up to eight viewers. Up to eight screens are supported on either app. The display and PIN survive ordinary viewer disconnects; the last viewer leaving idles capture. Shared source edits affect all attached active viewers and are broadcast after serialized source restart. Each local viewer entry retains its own target, PIN, settings, diagnostics and session. Closing a viewer display only detaches that subscriber.

Every media QUIC stream starts with a `MediaStreamHello` containing the protocol version, screen ID, generation, and lane (`video`, `audio`, `input`, `output`, or `diagnostics`). Video packets repeat their screen ID and generation in `MediaHeader`, along with sequence, timestamp, keyframe, and frame metadata. This gives the receiver enough information to validate channel identity and reject stale or mismatched packets. The control stream carries per-screen capture status and periodic sent-frame, drop, and bitrate statistics; media payload never shares the control stream.

## Lifecycle and cleanup

Control frames are read by a persistent reader task so another event winning `select!` cannot discard a partial message. Clean media FIN is distinguished from a truncated packet. Video notifications and statistics are generation-scoped; late events cannot close a replacement stream. Removed screen IDs are remembered until the session ends, so a delayed settings acknowledgement cannot recreate a deleted screen. Viewer settings revisions are captured with each edit, and an older acknowledgement cannot overwrite a newer pending edit.

Explicit disconnect, transport loss, control EOF or malformed control data, and app quit all close local video windows, cancel media tasks and pending screen operations, and reset session UI state. Host capture guards signal cancellation on drop. Host media workers are supervised so an unexpected task panic becomes a screen/generation-scoped failure; viewer receiver guards similarly report a panic and unregister the affected lane. Host transport closure is sent before waiting for macOS capture cleanup. Closing a receiving lane wakes the sender even when there are no video packets. Screen removal wins over queued restarts and returns selected-screen navigation to the list. Duplicate removal requests are rejected, and failed requests are shown with per-screen feedback.

Each Linux screen uses a supervised native child mode of the viewer executable. The child owns its winit window, direct decoder and GPU resources; GPUI and network controls stay in the parent. Only bounded compressed video and diagnostics cross versioned private Unix-socket IPC. Startup waits for a keyframe. Restart waits for the old child to be reaped. Normal shutdown requires an acknowledgement and process exit; an abnormal exit or six-second shutdown deadline fails only that screen and terminates/reaps the isolated child. The parent contains native crashes; it cannot guarantee that a GPU driver will never fault. Discovery handles host updates/removals and stops on app quit. Audio/input/output lanes remain reserved and unsupported; they are rejected without closing a valid video lane.

## Settings

The host provides Responsive, Balanced, and Detailed presets. The viewer uses the host's selected preset by default. Selecting a preset in the viewer requests a shared source change for that host screen, affecting its active subscribers. The codec submenu exposes orientation, frame rate, bitrate, keyframe interval, H.264 profile, and low latency rate control. Changes to display settings take effect immediately on the selected screen after its media pipeline restarts. **Save custom preset** writes to the local `airbridge/presets.json` configuration file on that machine.

The current session sends video packets with sequence numbers and timestamps; the protocol retains an audio packet type for later use. The host currently sends H.264. On Linux, the viewer probes hardware decoders in this order: NVIDIA NVDEC, VA-API (commonly Intel and AMD), then V4L2 (embedded SoCs). If none can open the stream, it uses OpenH264 software decoding. Hardware decoding and display rendering are separate choices: Vulkan DMA-BUF import can keep supported VA-API frames on the GPU, while unsupported imports and NVDEC CUDA frames currently use the decoder's CPU readback path before GPU presentation.

## Current limits

- The virtual display uses macOS's private `CGVirtualDisplay` API. It can change between macOS releases and does not suit Mac App Store distribution.
- Up to eight host screens and eight viewers per host screen. Each viewer-local entry connects to one host screen.
- Linux H.264 decode uses NVDEC, VA-API, or V4L2 when available, with OpenH264 software fallback. AV1 is not yet negotiated by the host/viewer stream, even though NVDEC support exists in the backend dependency.
- Linux VA-API DMA-BUF frames can be imported into Vulkan for GPU rendering when the decoder and presentation GPU/driver support sharing the exported surface. NVIDIA NVDEC is hardware accelerated, but its CUDA output currently requires a CPU readback before wgpu presentation.
- Each screen/lane currently uses an ordered QUIC stream. Packet loss can delay later frames within that lane, while other lanes continue independently. QUIC datagrams or frame-level retransmission policy are future latency improvements.
- The common codec ranking still describes future negotiation preference; this session currently streams H.264 only.
- The VSync setting is stored with presets; explicit display presentation control is not wired to it yet.
- Actual Linux linking and GPU/driver behavior still need validation on Fedora. Shared Rust/GPUI and host release builds are checked on macOS; no UI was launched during this correction.

The host's TLS certificate and private key are kept under the local `airbridge` configuration directory. The private key is mode `0600` on Unix. PIN authentication uses SPAKE2 and confirms the result against the QUIC TLS exporter before starting the media stream.

## Temporary stream diagnostics

In stream details, open **Stats** below Video settings. Opening requests a new instrumented stream generation; closing returns to the normal compiled path unless the other peer still has Stats open. Return to the list also closes the local subscription. Both apps must be rebuilt for protocol v5. Normal and instrumented capture/send/receive/decode/render functions are separately monomorphized from constant-generic implementations; the normal processing path has no timing collection branch. Routine sent/received-frame progress logs were removed; lifecycle/errors remain and console/file writes run asynchronously.

Stats retain 60 one-second samples with compact bitrate/FPS/RTT history charts. RTT jitter is the change between successive RTT samples, not RTP interarrival jitter; QUIC loss/congestion counters are connection-wide. Audio fields are reserved and unavailable. **Frame rows** retain at most 20 sampled frames (about one per second); **Stage averages** update fixed-size incremental accumulators without using the frame history. Freeze/Resume holds a readable snapshot, and Capture next 20 requests consecutive capture callbacks, then freezes after the observed stages conclude. In overloaded diagnostics queues, observations may be discarded and the loss counter increases; video never waits for diagnostics delivery.

Times use monotonic clocks. Four-timestamp probes every two seconds estimate the host-relative timeline, with RTT/2 uncertainty plus a conservative drift allowance; unsynchronized cross-machine starts are unavailable. Each traced frame keeps its clock mapping for its entire lifetime. Transfer timing includes QUIC writing, network transit and receiver scheduling, not pure wire latency. A successful QUIC write means accepted by the transport, not delivered. GPU completion is callback observation, and presentation submission means queued to the compositor: actual monitor scanout and partial rendering are explicitly unavailable. Five-second pending-stage timeouts and cancellation/failure reasons remain visible.

Host Stats selects a subscriber from the connected viewer list. Source tracing is shared when any subscription is active, while send timing, clocks, loss and bounded history remain per session. Fanout queues hold two compressed packets per viewer; overflow waits for a keyframe. Source queues hold four packets, worker admission holds one, and transport receive windows are bounded. IPC queue timing is available in instrumented mode.


## Capture failure and ownership audit

Screen Recording preflight gates capture and display preparation. Use the explicit permission action in host screen details, grant access to the executable/bundle actually launched, then restart. CoreGraphics API lookup failure, stream-create NULL, stream-start CGError, first-frame timeout and stop failure are reported as separate phases. No audio capture is enabled.

The native callback queue and callback context remain owned until stop is confirmed and the callback queue drains. Unconfirmed cleanup retains the entire producer/display and blocks new capture/display work until host restart. A stuck in-process macOS operation is not forcibly freed; the screen records the retained-resource failure. The Linux supervisor has a stronger isolation boundary because its native resources live in a child process.

The audited implementation plan and lifecycle/UI map are in [screen-ownership-and-lifecycle-plan.md](docs/screen-ownership-and-lifecycle-plan.md). Both apps must be rebuilt for protocol v5.
