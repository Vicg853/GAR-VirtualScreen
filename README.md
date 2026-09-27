# AirBridge

AirBridge is a native Mac to Linux extended display experiment. The Mac host creates a virtual display, captures its video, and sends it over an encrypted QUIC connection. The viewer discovers the host on the local network, authenticates with a six digit PIN, decodes the video, and shows the result in a GPUI window. Audio is temporarily disabled while the display path is being tested. Input sharing is outside this project's current scope.

## Build

The macOS host needs Rust, Xcode, and the Xcode Metal Toolchain. The first launch requires macOS Screen Recording permission; restart the host after granting it.

```sh
cargo run -p bridge-host --release
```

The Linux viewer needs Rust, FFmpeg, and the libraries required by GPUI for Wayland or X11. On Fedora, install the build dependencies and FFmpeg through your configured repositories, then run:

```sh
cargo run -p bridge-viewer --release
```

Start the Mac host, then start the viewer. Select the discovered Mac, type the PIN shown in the host window, and click **Connect**. If discovery fails, enter the Mac's IP address in the manual address field. UDP port **44990** and local mDNS traffic must be allowed between the machines.

## Settings

The host provides Responsive, Balanced, and Detailed presets. The viewer uses the host's selected preset by default. Selecting a preset in the viewer overrides the host's selection for that connection. The codec submenu exposes orientation, frame rate, bitrate, keyframe interval, H.264 profile, and low latency rate control. Changes to display settings take effect on the next connection. **Save custom preset** writes to the local `airbridge/presets.json` configuration file on that machine.

The current session sends video packets with sequence numbers and timestamps; the protocol retains an audio packet type for later use. The host currently advertises VideoToolbox H.264 hardware preferred and software modes. The viewer asks FFmpeg to select hardware decoding in the preferred mode and can use software decoding. The common codec ranking already describes the intended order: hardware AV1, hardware H.264, software AV1, then software H.264. A codec is advertised only after its implementation is available.

## Current limits

- The virtual display uses macOS's private `CGVirtualDisplay` API. It can change between macOS releases and does not suit Mac App Store distribution.
- Only one viewer and one virtual display are active at a time.
- The video viewer currently moves decoded frames through system memory into GPUI's GPU image renderer. Direct Vulkan or DMA buffer import is still to be implemented.
- The current media path uses an ordered QUIC stream. Packet loss can delay later frames. Independent video frame streams or datagrams are a future latency improvement.
- AV1 is represented in negotiation but has no encoder or decoder path yet.
- The VSync setting is stored with presets; explicit display presentation control is not wired to it yet.
- The Linux build and a two machine streaming session still need validation on Fedora hardware. The viewer has been compiled and launched on macOS to check the shared Rust and GPUI code paths.

The host's TLS certificate and private key are kept under the local `airbridge` configuration directory. The private key is mode `0600` on Unix. PIN authentication uses SPAKE2 and confirms the result against the QUIC TLS exporter before starting the media stream.
