# GPU stream engine implementation audit

Date: 2026-10-05. Scope: the Vulkan presentation implementation and direct software fallback requested in the GPU stream engine plan. This is a source/build/test audit, not a Linux GPU runtime certification.

## Implemented flow

1. QUIC receives bounded, complete Annex-B H.264 access units on the CPU. Packet sequence, source monotonic PTS, screen and generation remain distinct. Network protocol v6 declares progressive 8-bit 4:2:0 I/P-only H.264. Inline SPS/PPS are validated, including coded extent, crop, reference count, matrix/range and slice type. Malformed or unsupported input becomes a scoped stream error.
2. A supervised native child selects `StreamEngine<H264, VulkanH264, DEBUG>` or `StreamEngine<H264, SoftwareH264, DEBUG>` once. Supported codec/API implementations are sealed. Normal and measured functions are separately compiled. Backend selection and graphics timestamp capability selection do not run per picture.
3. Hardware parsing creates reference/decode instructions on the CPU. Compressed data is written into aligned coherent Vulkan input buffers; at most four native submissions remain pending. Timeline completion retires input and command resources asynchronously. No host mapping of decoded pixels occurs in this engine.
4. Vulkan decodes into its reference/output images. A GPU-only copy creates an independent NV12 presentation lease, including visible crop offsets. This copy is intentional: a reference image cannot be overwritten while decoding or rendering still requires it. The implementation does not claim an unconditional zero-copy DPB handoff.
5. Graphics receives an explicit timeline wait and retains its semaphore until that wait completes. Decoded images are sampled directly into the swapchain through NV12/I420 fragment conversion. Only GPU handles and numeric metadata cross the decoder/display thread boundary. No intermediate CPU RGBA surface or GPU-to-CPU decoded readback exists in the app path.
6. The newest completed candidate replaces older unpresented output. GPU callbacks retain each displayed image until reads finish. At most two graphics submissions are in flight. Reference pictures are preserved independently of presentation drops. Compressed overload requires recovery at a keyframe rather than silently breaking dependencies.
7. Software uses OpenH264 directly, uploads borrowed Y/U/V planes once into recycled GPU textures, and uses the same Vulkan presentation shader. No FFmpeg subprocess or generic moq decoder dispatch remains.

## Ownership and limits

- Native input submissions: four. Compressed picture size: bounded by protocol and 8 MiB aligned native input limit.
- Latest decoded-output mailbox: one; displaced candidates are released/count as skipped.
- Graphics submissions: two; GPU timestamp observations: two.
- Software output permits: eight, including cached textures; cached pool: four.
- Native output cache: bounded at eight images; allocator permits account actual Vulkan buffers/images/session allocations up to 256 MiB per screen. Software plane permits cap estimated texture planes at 128 MiB per screen.
- These totals exclude swapchains, wgpu internals, driver-private allocations and other app memory. Eight screens bound the sum of per-screen pools; there is no cross-process dynamic shared budget.
- Picture metadata correlation and trace correlations are bounded. Normal and measured paths use the same real PTS. Matrix/range metadata is retained per picture.
- Native graphics queue handles are independent of decode/copy handles. If a queue family cannot supply an independent graphics handle, hardware initialization fails explicitly and auto mode uses software. Native queues retain their existing serialized timeline dependency chain.
- Decoder submission guards wait before destroying in-flight resources on early errors/unwinding. Retirement waits for native completion; presentation leases wait for graphics completion.

## Lifecycle handling preserved and extended

- Startup/capability rejection: try compatible Vulkan Video/presentation adapters, report rejection, select software in auto mode. Forced Vulkan reports the error. Decode initialization errors close the affected lane.
- First generation/recovery: accept an IDR/keyframe before dependent frames. Explicit no-reordering output avoids retaining the final picture awaiting a future access unit.
- Settings or diagnostics change: generation restart; stop admission, release output candidates, wait for graphics completion, acknowledge consumer release, then reset/drop decoder resources and wait for recovery.
- Window close, screen removal, peer failure/disconnect or app quit: close admission and consumers, quiesce graphics, release mailbox/current frame, release decoder ownership barrier, join/reap native execution, close window and update scoped status.
- Parse/decode/copy/presentation/device errors: error status/log forwarded asynchronously to parent; parent retains controls/retry while closing the affected native stream.
- Native GPU no-progress: pending native work is monitored even below full queue capacity; five seconds without completion reports failure. Parent process supervision bounds stuck native cleanup separately.
- Panic/crash/hang: child isolation and parent reap/watchdog remain. This is containment, not a guarantee that native drivers cannot fault.

## Diagnostics

- Successful stage averages exclude dropped/failed/cancelled/timed-out/unavailable samples; each outcome is counted separately.
- Monitor scanout is unavailable rather than assigned reception time.
- Decode/output availability includes hardware copy completion; it is not presented as exact GPU decode execution duration.
- Input upload and command submission, output candidate wait, GPU view preparation, surface acquisition, GPU render timestamp duration, callback completion and compositor submission have distinct observations.
- GPU render duration uses timestamp queries only where supported. Its displayed start is anchored to CPU submission, not a calibrated GPU start; unavailable queries remain unavailable.
- Presented FPS counts first compositor submission of each picture separately from decoded FPS. Per-frame age is an instrumented estimate through compositor submission, with clock uncertainty, not scanout latency.
- Worker lifecycle/error logs are cursor exported over bounded IPC batches. No per-packet console logs are introduced. Existing bounded graph/frame-row/average views and async collection remain.

## Plan phase audit

| Phase | Status | Evidence / remaining work |
|---|---|---|
| A: measurement | Implemented in code; runtime diagnosis pending | Successful averages, unavailable scanout, real PTS, decoded/presented distinction, queue age; affected Linux run needed to locate the two-second delay |
| B: typed engines | Implemented | Sealed pairs and compiled observation modes; unsupported-pair compile-fail doctest |
| C: direct software | Implemented, CPU/NOOP tested | OpenH264 fixtures, direct plane writes and repeated reset/recycling tests |
| D: native Vulkan H264 | Implemented, compile checked | Patched `gpu-video` 0.4.0; no physical Vulkan decode validation on this Mac |
| E: freshness/pooling | Implemented; driver validation pending | Latest mailbox, timeline/graphics leases, admission and allocator limits, release barrier tests |
| F: NVDEC/VAAPI bridges | Not implemented | Requires separate device/format/external-memory/semaphore interoperability work and real-driver verification; former generic readback paths removed |
| G: diagnostics/budgets | Implemented; physical measurements pending | New stages/counters, async queries, bounded logs/pools, aggregation tests |
| H: regression/hardware | Nonvisual checks passed; Linux hardware pending | See checks below |

## Checks performed

- `cargo test --workspace`: 52 tests passed (core 22, host 5, display operations 4, viewer 21).
- Supplemental actual-source native check: 22 tests and one compile-fail doctest passed. Tests use actual OpenH264 with NOOP graphics, validate WGSL, bounded metadata/output, PTS parity, decoder reset cycles, barriers, and child clean/crash/hang lifecycle. No window is opened.
- Vendored no-reordering sorter test: passed for 1,000 completed pictures, even with SPS reorder capacity of 16.
- macOS workspace release build and host `.app` packaging succeeded; `codesign --verify --deep --strict` passed. The bundle is not launched.
- Linux `cargo check --target x86_64-unknown-linux-gnu`: blocked at native dependencies because `x86_64-linux-gnu-gcc` is absent. A Rust target alone does not supply the C toolchain/sysroot.
- No VM, container, UI launch, or visual checking was used.

## Remaining acceptance evidence

The complete plan is **not** certified complete. NVDEC/VAAPI engines remain unimplemented. Actual Linux linking, Vulkan validation layers/driver stress, Wayland/X11 presentation, sustained resource measurements, color/crop appearance, multi-screen behavior under load and an affected-viewer latency capture remain unverified. Unsupported hardware currently uses explicit software fallback; older NVIDIA cards are not promised a GPU-resident hardware engine.

Rebuild both peers for protocol v6. Run once with auto selection, optionally compare `AIRBRIDGE_DECODER=software` and supported forced Vulkan, and collect instrumented frame rows/stage averages to confirm where remaining frame age occurs. No claim that the original two-second symptom is fixed is justified before that run.
