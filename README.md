# AirBridge

AirBridge is a native Mac to Linux extended display experiment. The Mac host creates a virtual display, captures its video, and sends it over an encrypted QUIC connection. The viewer discovers the host on the local network, authenticates with a six digit PIN, decodes the video, and shows each stream in its own native video window. Audio is temporarily disabled while the display path is being tested. If a session requests audio, the host shows a soft warning and continues video-only. Input sharing is outside this project's current scope.

## Build

The macOS host needs Rust, Xcode, and the Xcode Metal Toolchain. The first launch requires macOS Screen Recording permission; restart the host after granting it.

```sh
cargo run -p bridge-host --release
```

The Linux viewer needs Rust 1.95.0 or newer, a C/C++ toolchain for OpenH264, the GPUI Wayland/X11 development libraries, and a Vulkan loader with a working GPU driver. The linker needs the development versions of `libxkbcommon` and `libxkbcommon-x11`. The decoder no longer builds `moq-vaapi` or its bindgen/Clang bindings.

```sh
# Fedora
sudo dnf install gcc gcc-c++ libxkbcommon-devel libxkbcommon-x11-devel vulkan-loader-devel

# Ubuntu / Debian
sudo apt install build-essential libxkbcommon-dev libxkbcommon-x11-dev libvulkan-dev
```

Then run:

```sh
cargo run -p bridge-viewer --release
```

Start the Mac host, then start the viewer. On the host, add/select a screen and open its details to see its PIN. On the viewer, add a local screen, select the discovered host screen inside its details, enter that screen's PIN, and click **Connect**. If discovery fails, enter the Mac's IP address in the manual address field. UDP port **44990** and local mDNS traffic must be allowed between the machines.

The connection uses QUIC keepalive packets every five seconds during capture setup. After PIN authentication, a dedicated bidirectional control stream stays separate from media. Each screen and each media lane uses its own labeled QUIC unidirectional stream with independent async sender/receiver tasks. A stalled or restarted screen lane therefore does not block another screen's media lane. Audio and future input/output lanes have reserved labels but are not implemented yet. Both apps must be rebuilt together because the wire protocol is version 6. A virtual display appearing in macOS Display Settings confirms display creation, but capture must still start before video can arrive.

The host's video-only path uses CoreGraphics display streams, so it does not construct a ScreenCaptureKit stream. ScreenCaptureKit initialized a CoreAudio clock even with audio capture disabled and stalled for about 150 seconds on one Mac. The CoreGraphics display-stream API is marked obsolete by the macOS 15+ SDK and may be removed in a future macOS release; the host reports a visible error if this API is unavailable.

### App launchers

On macOS, run `./scripts/package-macos.sh` to build `dist/AirBridge Host.app`. Open the bundle from Finder and drag it into the Dock if desired. macOS may ask for Screen Recording permission on the first launch; restart the app after granting it.

If macOS still reports that capture was denied, quit all host instances. In **System Settings → Privacy & Security → Screen & System Audio Recording** (called **Screen Recording** on older macOS), remove the stale AirBridge entry, add the exact `AirBridge Host.app` you launch, and enable it. Relaunch from Finder. The startup log shows the actual executable path and the Screen Recording preflight result. Permission given to a terminal-launched binary may not apply to the app bundle.

The packaging script uses ad hoc signing by default, so rebuilding can invalidate saved permission. Build first, then grant access. To sign subsequent builds with a consistent code signing certificate, use `AIRBRIDGE_SIGNING_IDENTITY="Your certificate name" ./scripts/package-macos.sh`. Use the same certificate and bundle identifier for subsequent builds; switching from ad hoc signing requires granting permission to the newly signed app.

On Linux, run `./scripts/install-linux.sh` to build and install the viewer into `~/.local/bin` with an `AirBridge Viewer` application launcher in `~/.local/share/applications`. Launch it from your desktop environment's application menu. The script installs for the current user and does not require root.

The host and viewer use GPUI Kit controls with themed Buttons, editable Form/Input fields and per-screen selection feedback. The viewer opens a compact controls widget and an independent native video window for each local screen. **+ Add screen** creates a local screen entry; select it to choose one discovered host screen (or enter a manual address), its screen ID and its six-digit PIN. Each local entry owns one host connection. The host creates screens with independent PINs and lists the viewers attached to the selected screen. **← Screens** / **← Screen list** returns to the corresponding list. Settings, presets, statistics, logs, **Show display window**, **Restart/Retry stream** and removal belong to screen details. **Cancel connection** appears while connecting; **Disconnect this screen** applies to that local entry.

Closing a native video window detaches that viewer from its host screen. The host screen and its other viewers remain independent. Removing a screen on the host closes its viewer window. A video lane failure closes its display and keeps its control row available for retry or removal. The primary screen can be removed too; an empty screen list remains connected and can add screens again. Closing the controls widget quits the app and tears down the whole session. Widgets can be dragged by the header, resized at the edges, and minimized. **Show display window** brings an existing live window to the front. Both widgets have expandable **Logs** sections; messages also go to the terminal and the displayed log file.

The native video window uses its own wgpu renderer, separate from GPUI, and keeps one compressed packet waiting for the decoder and one decoded frame waiting for presentation. If decoding outruns painting, the older unpainted frame is skipped. The statistics inspector reports the skipped-frame count so an overloaded viewer is visible.

## Wire protocol v8

A viewer connection can authenticate a saved host session using its account and password. One authenticated QUIC connection attaches the session's screens, each through an independent media lane and shared host source actor. The contract has a stable session ID and host ID; each viewer has a stable locally persisted device ID. Host-owned source settings and display arrangement are separate from viewer-owned display window preferences. Request IDs, settings revisions and capture generations scope later messages. Temporary screens that do not belong to a saved session still support the six-digit PIN flow. Session screens require their session account/password and do not admit PIN authentication.

Each host screen owns one prepared virtual display and one capture/encoder source shared by up to eight viewers. Up to eight source screens are supported on the host. The display survives ordinary viewer disconnects; the last viewer leaving idles capture. Shared source edits affect all attached active viewers and are broadcast after serialized source restart. Closing one viewer display detaches only that screen, retaining the host source and the other screens in the same session. Manual session disconnection pauses automatic reconnection until an explicit connection or auto-connect action.

Every media QUIC stream starts with a `MediaStreamHello` containing the protocol version, screen ID, generation, and lane (`video`, `audio`, `input`, `output`, or `diagnostics`). Video packets repeat their screen ID and generation in `MediaHeader`, along with sequence, timestamp, keyframe, and frame metadata. This gives the receiver enough information to validate channel identity and reject stale or mismatched packets. The control stream carries per-screen capture status and periodic sent-frame, drop, and bitrate statistics; media payload never shares the control stream.

## Lifecycle and cleanup

Control frames are read by a persistent reader task so another event winning `select!` cannot discard a partial message. Clean media FIN is distinguished from a truncated packet. Video notifications and statistics are generation-scoped; late events cannot close a replacement stream. Removed screen IDs are remembered until the session ends, so a delayed settings acknowledgement cannot recreate a deleted screen. Viewer settings revisions are captured with each edit, and an older acknowledgement cannot overwrite a newer pending edit.

Explicit disconnect, transport loss, control EOF or malformed control data, and app quit all close local video windows, cancel media tasks and pending screen operations, and reset session UI state. Host capture guards signal cancellation on drop. Host media workers are supervised so an unexpected task panic becomes a screen/generation-scoped failure; viewer receiver guards similarly report a panic and unregister the affected lane. Host transport closure is sent before waiting for macOS capture cleanup. Closing a receiving lane wakes the sender even when there are no video packets. Screen removal wins over queued restarts and returns selected-screen navigation to the list. Duplicate removal requests are rejected, and failed requests are shown with per-screen feedback.

Each Linux screen uses a supervised native child mode of the viewer executable. The child owns its winit window, direct decoder and GPU resources; GPUI and network controls stay in the parent. Only bounded compressed video and diagnostics cross versioned private Unix-socket IPC. Startup waits for a keyframe. Restart waits for the old child to be reaped. Normal shutdown requires an acknowledgement and process exit; an abnormal exit or six-second shutdown deadline fails only that screen and terminates/reaps the isolated child. The parent contains native crashes; it cannot guarantee that a GPU driver will never fault. Discovery handles host updates/removals and stops on app quit. Audio/input/output lanes remain reserved and unsupported; they are rejected without closing a valid video lane.

## Settings

The host provides Responsive, Balanced, and Detailed presets. The viewer uses the host's selected preset by default. Selecting a preset in the viewer requests a shared source change for that host screen, affecting its active subscribers. The codec submenu exposes orientation, frame rate, bitrate, keyframe interval, H.264 profile, and low latency rate control. Changes to display settings take effect immediately on the selected screen after its media pipeline restarts. **Save custom preset** writes to the local `airbridge/presets.json` configuration file on that machine.

The host currently sends progressive 8-bit 4:2:0 H.264 without B-frame reordering. Each video lane declares this contract, and the viewer carries the source PTS through its private worker IPC in both normal and instrumented modes. Inline SPS/PPS supply crop and SDR color metadata.

At screen startup, the native worker selects a compiled `StreamEngine<H264, API, INSTRUMENTED>` once. It first attempts native Vulkan Video decoding on the presentation device. If initialization fails, it reports the reason and selects direct OpenH264 software decoding with Vulkan presentation. Set `AIRBRIDGE_DECODER=software` to force software, or `AIRBRIDGE_DECODER=vulkan` to require native Vulkan Video and report an error instead of falling back. No decoded-image CPU readback is used by the hardware engine. Software uploads borrowed decoder Y/U/V planes directly into reusable textures once; a fragment shader converts them into the swapchain.

The vendored `gpu-video` adapter submits decoding asynchronously and polls timeline completion. Its compressed input ring holds at most four submissions, independent output images are retained by leases, and the latest completed candidate replaces older unpresented output. Rendering permits at most two submissions in flight. Native video allocations are capped at 256 MiB per screen; software plane storage is capped at 128 MiB and eight leased/cached pictures. Swapchains, driver-private memory and other application allocations are outside these accounting values. The eight-screen application limit bounds the sum of per-screen pools; there is no dynamically shared memory budget across child processes.

## Current limits

- The virtual display uses macOS's private `CGVirtualDisplay` API. It can change between macOS releases and does not suit Mac App Store distribution.
- Up to eight host screens and eight viewers per host screen. Each viewer-local entry connects to one host screen.
- Native Vulkan H.264 decoding requires suitable profile/format support and independent native/graphics queue handles. Unsupported devices use software decoding. This does not guarantee Vulkan Video support on a GTX 1060 or every Intel/AMD driver.
- NVDEC/CUDA, VAAPI/DMA-BUF and V4L2 hardware engines are not part of this implementation. Their former generic readback paths have been removed. AV1 and non-Vulkan presentation are future work.
- SDR BT.709/BT.601 matrix/range and cropped progressive 4:2:0 are handled; HDR/BT.2020 and interlaced or higher-bit-depth streams are rejected.
- Each screen/lane currently uses an ordered QUIC stream. Packet loss can delay later frames within that lane, while other lanes continue independently. QUIC datagrams or frame-level retransmission policy are future latency improvements.
- The common codec ranking still describes future negotiation preference; this session currently streams H.264 only.
- The VSync setting is stored with presets; explicit display presentation control is not wired to it yet.
- Actual Linux linking and GPU/driver behavior still need validation on Fedora. Shared Rust/GPUI and host release builds are checked on macOS; no UI was launched during this correction.

The host's TLS certificate and private key are kept under the local `airbridge` configuration directory. The private key is mode `0600` on Unix. PIN authentication uses SPAKE2 and confirms the result against the QUIC TLS exporter before starting the media stream.

## Rust toolchain

`rust-toolchain.toml` pins Rust 1.95.0 and Cargo declares Rust 1.95 as the minimum. GPUI pre 0.3.8 uses `std::hint::cold_path`, stabilized in Rust 1.95; older stable compilers report E0658. With rustup, install the pinned toolchain before building:

```sh
rustup toolchain install 1.95.0 --profile minimal
cargo run -p bridge-viewer --release
```

Run Cargo inside this repository so its toolchain file is applied. No nightly compiler or unstable-feature override is required.

## Temporary stream diagnostics

In stream details, click **Open statistics** below Video settings. Opening the inspector requests a new instrumented stream generation; closing it returns to the normal compiled path unless another peer still has its inspector open. Widget navigation leaves open inspectors and their subscriptions active. Both apps must be rebuilt for protocol v8. Normal and instrumented capture/send/receive/decode/render functions are separately monomorphized from constant-generic implementations; the normal processing path has no timing collection branch. Routine sent/received-frame progress logs were removed; lifecycle/errors remain and console/file writes run asynchronously.

**Open statistics** in the screen controls opens a separate resizable Kit DataTable inspector and enables temporary instrumentation. Statistics and timing controls appear only in this window. The identity column stays pinned while scrolling; columns can be resized. **GPU / display stages** filters the table for decoding, queues, uploads, swapchain acquisition, GPU work and presentation. **Start after capture (avg)** is a cumulative offset; **Stage duration (avg)** is time within the stage. Do not add the start offsets. Closing the inspector releases this screen's diagnostics subscription; instrumentation remains active if another peer is still subscribed. Removing/disconnecting a source clears the inspector's data and disables its stream actions.

Stats retain 60 one-second samples with compact bitrate/FPS/RTT history charts. RTT jitter is the change between successive RTT samples, not RTP interarrival jitter; QUIC loss/congestion counters are connection-wide. Audio fields are reserved and unavailable. **Frame rows** retain at most 20 sampled frames (about one per second); **Stage averages** update fixed-size incremental accumulators without using the frame history. Freeze/Resume holds a readable snapshot, and Capture next 20 requests consecutive capture callbacks, then freezes after the observed stages conclude. In overloaded diagnostics queues, observations may be discarded and the loss counter increases; video never waits for diagnostics delivery.

Times use monotonic clocks. Four-timestamp probes every two seconds estimate the host-relative timeline, with RTT/2 uncertainty plus a conservative drift allowance; unsynchronized cross-machine starts are unavailable. Each traced frame keeps its clock mapping for its entire lifetime. Transfer timing includes QUIC writing, network transit and receiver scheduling, not pure wire latency. A successful QUIC write means accepted by the transport, not delivered. GPU completion is callback observation, and presentation submission means queued to the compositor: actual monitor scanout and partial rendering are explicitly unavailable. Five-second pending-stage timeouts and cancellation/failure reasons remain visible.

Host Stats selects a subscriber from the connected viewer list. Source tracing is shared when any subscription is active, while send timing, clocks, loss and bounded history remain per session. Fanout queues hold two compressed packets per viewer; overflow waits for a keyframe. Source queues hold four packets, worker admission holds one, and transport receive windows are bounded. IPC queue timing is available in instrumented mode.


## Capture failure and ownership audit

Screen Recording preflight gates capture and display preparation. Use the explicit permission action in host screen details, grant access to the executable/bundle actually launched, then restart. CoreGraphics API lookup failure, stream-create NULL, stream-start CGError, first-frame timeout and stop failure are reported as separate phases. No audio capture is enabled.

The native callback queue and callback context remain owned until stop is confirmed and the callback queue drains. Unconfirmed cleanup retains the entire producer/display and blocks new capture/display work until host restart. A stuck in-process macOS operation is not forcibly freed; the screen records the retained-resource failure. The Linux supervisor has a stronger isolation boundary because its native resources live in a child process.

The audited implementation plan and lifecycle/UI map are in [screen-ownership-and-lifecycle-plan.md](docs/screen-ownership-and-lifecycle-plan.md). Both apps must be rebuilt for protocol v8.

## GPU engine implementation and nonvisual checks

See [gpu-stream-engine-plan.md](docs/gpu-stream-engine-plan.md) and [gpu-stream-engine-implementation.md](docs/gpu-stream-engine-implementation.md) for the phase audit, resource contracts and remaining hardware validation. [The supplemental test tool](tools/native-video-check/README.md) compiles the actual native worker, engine and lifecycle source on macOS without opening windows. Its OpenH264 fixtures use a NOOP graphics backend; this checks CPU decoding, ownership, reset behavior and shader validity, not Vulkan execution or Linux presentation.

GPU timing separates surface acquisition, CPU preparation, GPU render timestamp duration and completion callback observation. Unknown monitor presentation is unavailable. Stage averages include successful samples only; failures and unavailable observations retain their own counts. The observed frame age ends at compositor submission, not scanout. Actual GPU driver tests and an instrumented run on the affected Linux viewer are required to locate and confirm removal of the reported two-second delay.

## GPUI Kit migration

The migration follows the official GPUI Kit and Design Guides skills. [Plan and verification checklist](docs/gpui-kit-controls-plan.md) and [implementation notes](docs/gpui-kit-controls-implementation.md) document component ownership, interaction flows and timing interpretation. GPUI Kit 0.7.1 supplies its matching GPUI/platform dependencies, including Wayland/X11 support. This controls change preserves the separate native GPU video engine.

## Saved host sessions

1. On the host, open **Sessions**, enter an account name and a password (no minimum; up to 256 bytes), and click **Save current screens as session**. This groups currently unassigned screens under one session ID and one password. Select a saved session before adding more host screens; those screens join that session. Source settings and virtual display arrangement are saved automatically. Sessions and registered viewer preferences are restored on host startup.
2. On the viewer, choose **+ Add host session**, select the discovered host, enter **Session account** and **Password / screen PIN**, turn **Remember credentials** on if desired, and connect. The screen ID is only used for temporary PIN connections; account login opens the session's screens. Remembering a login enables auto-connect, which can also be switched off independently.
3. Saved viewer sessions appear again on startup. Discovery matches the stable host identity, not a cached address or display name. Reconnect retries use bounded exponential backoff (at most 30 seconds). Both apps must be running. Disconnect/cancel pauses reconnection; **Forget saved login** removes persisted credentials, and removing a local session also forgets its saved login.
4. Viewer window sizes and positions are remembered separately from host source resolution. Wayland compositors may refuse explicit window positioning. Virtual display layout restoration uses CoreGraphics and reports a soft error if macOS refuses it.
5. **Keep host awake** and the viewer's local **Keep viewer awake** preference are independent session settings, disabled by default. The host also has a saved **Viewer lock policy** per session: **viewer choice** (default), **keep awake**, or **allow idle lock**. A host override is sent live to all connected viewers and disables their local toggle until the host returns to viewer choice; their local preference is preserved. “Allow idle lock” releases AirBridge's inhibitor; it does not force a desktop to lock. They inhibit idle sleep only while video is active, and release on disabling, failure, disconnect or shutdown. macOS uses a scoped `caffeinate -di` process; Linux uses the desktop ScreenSaver D-Bus inhibitor with GNOME SessionManager fallback. Desktop policy controls automatic locking; the app never unlocks an already locked desktop or overrides manual locks. Unsupported/denied inhibition is shown in the controls without terminating video.

`host-sessions.json` and `viewer-sessions.json` reside in the existing AirBridge config directory and are atomically replaced with Unix mode `0600`. The host stores a salted Argon2id-derived shared secret rather than the plaintext password. The viewer stores that secret only when remembering is enabled. Stored keys are authentication credentials and must be treated like passwords; these files are private local storage, not an encrypted keychain. Authentication uses SPAKE2 proof confirmation bound to the QUIC TLS exporter, protocol version, host ID, session ID and viewer ID. Neither the password nor the stored key is transmitted. Only host-owned source settings are accepted as source changes; viewer preference messages cannot edit another session's screens.

Implementation and verification: [persistent-sessions-plan.md](docs/persistent-sessions-plan.md).
