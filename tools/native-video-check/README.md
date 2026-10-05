# Nonvisual native engine checks

Run from the repository root:

```sh
CARGO_TARGET_DIR="$PWD/target/native-video-check" cargo test --manifest-path tools/native-video-check/Cargo.toml
```

This standalone tool includes the real `stream_engine.rs`, `native_video.rs`, `linux.rs` and `resource_lifecycle.rs` sources. It compiles the Vulkan API integration on macOS through wgpu Vulkan portability. It does not open UI windows, load a Vulkan device, launch a VM/container, or validate Linux WSI.

Tests exercise actual OpenH264 decode, source PTS parity in separately compiled observation modes, 100 decoder resets / thousands of pictures using the NOOP graphics backend, bounded output leases, Annex-B metadata parsing, malformed SPS rejection, WGSL validation, consumer release barriers, and native-worker clean/crash/hang supervision. A compile-fail doctest rejects unsupported codec/API combinations.

`cargo test --workspace` additionally checks protocol, IPC, statistics, control and host behavior. Passing these checks is not evidence of physical GPU decoding, external image lifetime correctness on a driver, color appearance, or end-to-end latency. Those require Linux runtime tests supplied by the user.
