# GPU-resident stream engine implementation plan

Date: 2026-10-05  
Status: Proposed; documentation only. Implementation has not started.

This records the plan discussed in chat, based on inspection of the current host,
viewer, native worker, rendering dependency, and telemetry code. The objective is
to remove unnecessary decoded-pixel round trips, bound resource retention, and
measure the actual source of visible latency.

## 1. Scope and constraints

- Receive and assemble compressed video on the CPU.
- For compatible hardware, transfer compressed pictures into decoder resources,
  decode on the GPU, and retain decoded pixels on the GPU through presentation.
- Keep a direct software decoder fallback, with one decoded-image upload.
- Use Vulkan for the display implementation in this phase. Native Vulkan Video,
  NVDEC with Vulkan interoperability, and VAAPI with Vulkan interoperability are
  distinct engines with distinct capability requirements.
- Select a concrete, compiled engine once when a stream generation starts.
- Enforce supported codec/API combinations at compile time.
- Preserve separate compiled normal and instrumented implementations.
- Preserve native worker process isolation and ordered lifecycle cleanup.
- Preserve host ownership: one screen, one PIN, one shared source, N viewers.
  A viewer entry connects to one host screen independently.
- Audio remains disabled. Reserve extension points for audio and future APIs;
  do not implement audio, Metal, or DirectX in this work.
- Builds and automated tests are permitted. No visual checking, UI launches,
  VMs, containers, or external build machines.

GPU residency is the requirement for the hardware path. A GPU-to-GPU copy is
acceptable when needed for ownership or format compatibility. Do not advertise
zero-copy behavior without verifying the actual resource path.

## 2. Current code findings

| Area | Current behavior | Required action |
| --- | --- | --- |
| Viewer network reception | Assembles a complete compressed CPU payload and moves its ownership into the native worker path | Keep bounded assembly and explicit picture metadata |
| Native worker IPC | Compressed payloads cross a private Unix socket into an isolated process | Preserve isolation; measure IPC separately |
| Decoder selection | General decoder catalog dispatches among NVDEC, VAAPI, V4L2, and OpenH264 | Replace per-picture generic backend dispatch with concrete engine implementations |
| Vulkan decoding | No application-native Vulkan Video decoder currently exists | Add capability probing and a real supported implementation |
| Graphics device | wgpu chooses a graphics adapter separately from decoder selection | Select decoder and presenter against the same physical device or verified interoperability |
| Decoded queue | Capacity one; a full queue drops the new frame and retains the old queued frame | Retain the newest eligible output without invalidating codec references |
| Event loop | Already receives a wake event when decoded output arrives | Measure scheduling and wake-to-consumption delay; do not assume a missing wake |
| Hardware rendering fallback | Rendering dependency can call `to_i420()` and upload CPU planes when import fails | Make GPU import support explicit; forbid hidden download in a GPU-resident engine |
| CUDA output | Dependency copies mapped decoder output into owned CUDA storage before unmapping | Respect decoder pool ownership; investigate external-memory interoperability |
| Surface retention | Rendering dependency has an unbounded completion channel retaining import leases | Audit and replace with bounded in-flight completion/resource accounting |
| Timestamp handling | Normal and instrumented paths currently use different sequence-derived decoder timestamps | Use the same real media timing and ordering in both modes; trace IDs remain separate |
| Rendering | Converts into an intermediate RGBA texture before drawing into the window | Prefer direct decoded-plane sampling and one color/scale pass where supported |
| Presentation | FIFO and desired maximum frame latency two | Query supported modes and measure acquisition/backpressure before choosing a low-latency policy |
| Statistics | Some counters count decoded frames rather than presented frames | Separate received, decoded, eligible, submitted, and presentation-confirmed counts |
| Timing averages | Failed/unavailable events can contribute misleading numeric averages | Separate successful timings from failures, incomplete stages, and unavailable measurements |

Relevant code:

- `crates/bridge-viewer/src/main.rs`: network lane reception and trace creation.
- `crates/bridge-viewer/src/linux.rs`: native worker supervision, IPC, shutdown.
- `crates/bridge-viewer/src/native_video.rs`: decode loop, output queue, renderer.
- `crates/bridge-core/src/telemetry.rs`: stages, frame rows, averages, outcomes.
- `moq-video 0.1.7`: decoder output ownership, renderer imports, CPU fallback,
  completion lease retention. Dependency changes must be reproducible in the
  repository, not edits to the Cargo registry cache.

## 3. Latency interpretation before optimization

The reported roughly 2.5-second decoded-frame queue timing is a strong reason to
investigate output age and resource waiting. It does not prove that upload itself
takes 2.5 seconds. Stage-relative start times and stage durations are different.
Subtracting independently aggregated averages can also mix different frame sets.

If the same frame starts upload at 2494.72 ms and reaches submission at 2498.06 ms,
that interval is 3.34 ms. The earlier delay must be measured separately. The
reported `249472ms` upload value also needs confirmation of its units/format.

Rust channels move owned buffers; they do not inherently deep-copy frame pixels.
Compressed IPC at the reported 331 kbps is not evidence of a multi-second copy
bottleneck. The hardware-to-CPU-to-GPU rendering fallback is a real path to remove,
but the source of the visible two-second delay remains unproven.

## 4. Target data flow

```text
QUIC reception and bounded picture assembly (CPU)
  -> validated encoded picture, ordering and codec metadata (CPU)
  -> bounded decoder input resources
  -> GPU hardware decode
  -> GPU reference pool + GPU display output pool
  -> newest eligible output selected using metadata and GPU completion
  -> GPU color conversion, crop, scaling and orientation
  -> swapchain / compositor presentation
```

Software fallback:

```text
CPU reception -> CPU codec parsing -> direct software decode
  -> one upload of decoded planes -> Vulkan rendering -> presentation
```

Compressed bytes cannot be reinterpreted as a decoded image through a reference
move. Zero-copy means sharing a compatible decoded GPU resource across decoder
and renderer while respecting ownership, synchronization, and lifetime rules.

## 5. Compiled engine architecture

Use one conceptual family:

```rust
StreamEngine<Codec, Api, ObservationMode>
```

Define sealed compatibility traits and associated implementation types. A
supported pairing provides its parser, decoder, resource pool, and presenter.
Unsupported pairs have no implementation; do not add a blanket implementation
that defers incompatibility to per-frame runtime checks.

Example conceptual combinations:

- H264 + native Vulkan Video + Vulkan presentation.
- H264 + NVDEC/CUDA interoperability + Vulkan presentation.
- H264 + VAAPI/DMA-BUF interoperability + Vulkan presentation.
- H264 + OpenH264 software decoding + Vulkan presentation.

These are candidate supported pairs, not claims that all are available on the
current GPU or already implemented. Add a pair only after its contracts are
validated. Keep codec choice separate from API choice, and allow future codec
markers without implementing unsupported combinations.

At startup:

1. Read the negotiated stream configuration and local capabilities.
2. Verify codec profile, bit depth, chroma format, dimensions, image usage,
   external-memory support, and presentation support.
3. Match decoder and graphics physical devices.
4. Select the supported concrete engine and normal/instrumented specialization.
5. Create its resources and run its concrete processing loop.

The startup dispatcher may branch. Steady-state processing should use concrete
types rather than a general backend enum or trait-object call for each picture.
Backend changes create a new generation after cleanup, with an explicit UI/log
reason. Do not silently download pixels inside a hardware-resident engine.

Normal and instrumented engines must use identical timestamps, ordering,
resource policies, and presentation behavior. Observation mode only changes
collection of diagnostic data.

## 6. Ownership and thread model

The viewer parent owns network/session state, UI state, and worker supervision.
The native child owns decoder, GPU resources, display event loop, and native
window. Keep the compressed IPC boundary for native fault containment.

Within the child, consolidate stream processing ownership where API constraints
allow. Use one primary submission/resource owner; add threads only for required
event-loop affinity, blocking decoder APIs, or independently bounded work.
Cross-thread transfer should move ownership/handles, not duplicate pixels.

Respect Wayland/X11 event-loop constraints, decoder callback constraints, and
GPU API thread affinity. A single-thread design must not block window events on
a decoder wait. GPU execution remains asynchronous even when submission and
resource bookkeeping are owned by one thread.

Audit whether wgpu can safely expose the required native Vulkan resources and
video operations. Otherwise use a compatible Vulkan video library or an explicit
Vulkan implementation such as ash. Do not assume wgpu's graphics API implements
Vulkan Video. Verify dependency versions and external ownership contracts before
choosing the implementation.

## 7. CPU reception and encoded pictures

Introduce an owned encoded-picture representation containing:

- Session, screen, stream generation, and picture identity.
- Compressed bytes, with bounded maximum size.
- Decode timestamp and presentation timestamp/order.
- Codec parameter revision and recovery/random-access information.
- Reference/dependency metadata produced by the codec parser.
- Coded dimensions, visible crop, orientation, and color information.
- Instrumented-only trace metadata in the diagnostic specialization.

Validate identity, payload size, dimensions, codec state, and generation before
admission. Transfer `Vec`/`Bytes` ownership rather than clone payloads. Keep CPU
H264 parsing: Vulkan Video needs codec parameters and reference metadata supplied
by the application. GPU decoding does not remove this CPU responsibility.

Add explicit protocol fields for required timing, codec parameter revisions,
color metadata, and recovery requests. Bump protocol version when wire contracts
change. Preserve independently scoped control/media channels and per-screen
errors. Host source changes affect all viewers of that shared source.

## 8. Compressed GPU input resources

For native Vulkan Video, allocate a bounded bitstream ring with the correct video
decode usage, profile, alignment, and size requirements. Model each slot as:

```text
Free -> CPU writing -> Ready -> Decoder reading -> Retired -> Free
```

Evaluate two legal paths:

1. Host-visible mapped decode input, with required flushing/coherency handling.
2. Host staging followed by a transfer into device-local decode input.

Choose from measured support and performance. Account for transfer completion,
queue-family ownership, barriers, and decode-read completion before reuse.
Bound both picture count and bytes; large pictures must not grow the ring.

NVDEC and VAAPI have their own compressed-input contracts. Do not route them
through a fake Vulkan bitstream buffer if their APIs require CPU input. Their
essential GPU-resident benefit is retaining decoded pixels on the GPU.

Ordinary QUIC reception still uses CPU memory. Direct NIC-to-GPU networking is
specialized and is outside this plan; no GPUDirect requirement is introduced.

## 9. Decode resources and output ownership

Keep three separately accounted pools:

1. Compressed input buffers.
2. Codec reference/DPB images.
3. Presentation output images or leases.

Reference requirements come from the codec and hardware capabilities. A display
queue of one or two entries does not imply the decoder only needs two images.
Query whether native Vulkan decode output and DPB may coincide or must be
distinct, and select legal formats/usages.

Preferred decoded-output paths:

1. Decode into a GPU image directly sampleable by the presenter.
2. Import a decoded image through supported external memory and synchronization.
3. Copy GPU-to-GPU into a presenter-owned compatible image when necessary.

For NVDEC, verify CUDA/Vulkan device matching, export/import compatibility,
semaphores, and decoder output retention. For VAAPI, verify DMA-BUF planes,
modifiers, device matching, fences, and image format support. Keep source leases
alive until GPU consumers finish. An import failure must cause explicit engine
failure/fallback selection after cleanup, not an undisclosed CPU readback.

The software engine decodes through a direct library and uploads decoded planes
once. It uses the same bounded presentation policy.

## 10. Replacement and priority policy

An output slot is replaceable only when all conditions hold:

- It is not required as a codec reference.
- No decode or copy command is writing it.
- No GPU rendering command is reading it.
- No external decoder/import lease still requires it.
- Its generation and ownership state permit retirement.

Use CPU bookkeeping plus GPU completion signals for these decisions. GPU image
analysis is not needed to decide ownership. Never overwrite an image because it
has merely not been displayed yet.

Priority order:

1. Reject obsolete generations.
2. Decode dependency-required pictures in valid decode order.
3. Observe codec presentation order and reorder requirements.
4. Present the newest completed, eligible output.
5. Retire older display candidates once safe, retaining necessary references.

Do not arbitrarily discard compressed H264 pictures. Under overload, either keep
dependency-safe decoding while skipping obsolete display output, or request a
recovery picture and reset the affected generation explicitly.

Replace the current full-queue behavior that retains an older output and drops
the newer arrival. Record distinct drop reasons for stale output, recovery,
admission pressure, obsolete generation, and decode failure.

## 11. Vulkan presentation

Sample decoded planes directly where supported. Apply crop, orientation, scale,
and color conversion in one GPU pass into the swapchain. Preserve matrix, range,
transfer function, primaries, and chroma placement; define explicit defaults only
when source metadata is absent. Avoid an intermediate RGBA texture unless needed
for API compatibility or another documented operation.

Use fences/timeline semaphores and asynchronous completion tracking. Do not wait
for the whole device to become idle per frame. Include external image acquire and
release synchronization. Query present modes rather than assuming mailbox is
available. Measure FIFO acquisition blocking and compositor backpressure.

Treat desired maximum frame latency as a hint, not a universal queue-size
guarantee. Presentation submission is not monitor scanout. Actual presentation
feedback is reported only when the platform supplies a valid measurement.

## 12. Resource budgets

Bound and expose:

- CPU assembly bytes and encoded pictures.
- IPC pending payloads.
- GPU compressed input bytes and slots.
- Decoder references and output slots.
- Imported image leases and external handles.
- Pending display candidates.
- GPU submissions, command resources, queries, and completion records.
- Diagnostic events and histories.

Apply budgets per stream and across simultaneous screens. Do not hide an
unbounded queue in completion workers or resource retirement. Backpressure must
have a defined outcome and recovery policy. Track allocated versus retained
resources and distinguish measured memory from estimates.

## 13. Logging and latency diagnostics

Keep per-packet logs out of normal operation. Log lifecycle changes, backend
selection, warnings, failures, and recovery summaries asynchronously through
bounded channels. Avoid synchronous log I/O on media processing paths.

Compile normal and instrumented processing functions separately. Opening timing
diagnostics restarts the affected stream generation into its instrumented engine;
closing it returns to normal through the same lifecycle. Normal frame functions
contain no runtime diagnostic-mode branch.

Instrument separate stages where applicable:

- Network first-byte reception and complete picture assembly.
- Native worker IPC queue and transfer.
- Engine admission and CPU codec parsing.
- Bitstream transfer submission/completion.
- Decode submission/completion.
- Output eligibility and display candidate age.
- External import/fence waits, or software upload.
- Swapchain acquisition.
- GPU render submission/completion.
- Presentation submission and available platform feedback.

Use asynchronous GPU timestamp queries when supported, with clock calibration
and uncertainty stated. CPU observation of a completion callback is not an exact
GPU execution timestamp. Preserve host/viewer clock synchronization and its
uncertainty; use local monotonic intervals for local durations.

Maintain three bounded displays:

1. Approximately twenty frame rows, with per-stage start, duration, outcome,
   failure reason, and incomplete state.
2. One row per stage with fixed-memory running averages and counts.
3. Separate bounded graph history for bitrate, frame rate, RTT, jitter, drops,
   occupancy, and related stream statistics.

Successful duration averages must only aggregate matching completed samples.
Show failed, cancelled, timed-out, pending, and unavailable outcomes separately.
Unavailable monitor presentation must have no fabricated numeric timestamp.
Scope correlation by session/screen/generation/picture, and prevent duplicate
terminal outcomes from inflating statistics.

Report received, decoded, presentation-submitted, and presentation-confirmed
rates separately. Include actual displayed candidate age, queue occupancy,
in-flight GPU work, engine/backend, memory budget, and drop reasons. Reserve
future audio statistic types without collecting audio data.

## 14. Lifecycle and failure handling

Model engine states explicitly:

```text
Probing -> Initializing -> Waiting for recovery -> Running
  -> Quiescing -> Closed
```

Startup, configuration changes, diagnostic mode changes, host errors, remote
removal, user close, transport loss, permission loss, decoder errors, GPU device
loss, worker crashes, and application exit must all enter a defined transition.

Shutdown dependency order:

1. Stop admitting new pictures and mark the generation obsolete.
2. Stop network/IPC producers for that generation and wake blocked consumers.
3. Resolve submitted GPU work and external synchronization within the worker's
   bounded shutdown policy.
4. Release display consumers, imported leases, and safe output resources.
5. Release decoder references, decoder sessions, and native resource owners.
6. Close the native window and terminate the child cleanly.
7. Send cleanup acknowledgement and reap the process.
8. Update UI state; only then create a replacement generation.

Adjust exact resource destruction order to each API's ownership contracts. Never
free a resource while callbacks or GPU commands can still access it. If native
cleanup cannot finish, the parent enforces the existing worker deadline and
terminates/reaps the child. A native crash must remain a scoped stream error,
not a crash of the viewer widget or unrelated streams.

## 15. Implementation phases

### A. Correct measurement and identify latency

- Fix timestamps, outcome aggregation, unavailable presentation values, and
  decoded-versus-presented counters.
- Measure individual frame age and waits at existing boundaries.
- Audit retained imports, completion queues, surface acquisition, and wake delay.
- Capture evidence locating the visible latency before attributing it to copies.

### B. Introduce typed engines

- Add codec/API markers, sealed supported-pair traits, and observation modes.
- Move capability selection to generation startup.
- Preserve process supervision, protocol scoping, and UI ownership.
- Define explicit backend rejection and fallback reasons.

### C. Establish software decode with Vulkan presentation

- Implement direct software decode and one plane upload.
- Establish bounded resources, correct media timestamps, colors, and latest
  eligible output policy using the shared engine contracts.
- Use this engine as a correctness fallback, not as hidden hardware readback.

### D. Add native Vulkan H264 decoding

- Probe device/profile/format/queue capabilities.
- Implement codec parsing, session parameters, aligned input ring, DPB/output
  resources, command submission, and synchronization.
- Decode and present without downloading decoded pixels.

### E. Enforce output freshness and safe pooling

- Implement reference-aware slot states and GPU completion retirement.
- Keep the newest eligible presentation candidate.
- Bound all resource pools and implement dependency-safe overload recovery.

### F. Add compatible NVDEC and VAAPI bridges

- Add compiled hardware engines only after verifying interoperability contracts.
- Match physical devices and handle formats, modifiers, external memory,
  semaphore/fence import, and lease lifetimes explicitly.
- Reject unsupported paths and choose an explicit supported fallback.

### G. Complete diagnostics and budgets

- Add GPU timing, fixed aggregates, bounded frame rows, and graph statistics.
- Implement mode-change restarts and async bounded event collection.
- Confirm normal and instrumented modes preserve identical media behavior.

### H. Regression and hardware validation

- Run permitted builds and automated tests.
- Clearly separate what is compile-tested, simulated, and validated on hardware.
- Do not claim Linux GPU behavior from macOS-only compilation.
- User supplies runtime/visual results; no VM/container/UI workaround.

## 16. Test and validation matrix

- Supported codec/API pairs compile; unsupported pairs fail to instantiate.
- Timestamp and frame ordering are identical across observation modes.
- Parameter revisions, reordered/reference pictures, recovery, and generation
  changes preserve valid decoder state.
- Slots cannot be reused while decode/render/reference/import consumers hold them.
- Slow rendering retains the newest eligible output without unbounded memory.
- Input overload does not discard required compressed dependencies silently.
- Memory stabilizes during prolonged streaming and repeated restart cycles.
- Multiple screens respect both per-stream and total resource budgets.
- Host error, transport loss, close, config restart, device loss, child crash,
  and cleanup timeout leave unrelated streams and controls usable.
- Color metadata and conversion cover range, matrix, crop, and orientation.
- Hardware mode performs no decoded-image CPU readback; software mode uploads
  decoded pixels once.
- Timing records distinguish CPU waits, GPU work, compositor submission, and
  unavailable physical presentation.
- Hardware validation covers supported NVIDIA, Intel, and AMD paths on Wayland
  and X11 where available. Capability rejection is also tested.

## 17. Acceptance checklist

- [ ] Real frame-age evidence explains or localizes the reported seconds of delay.
- [ ] Hardware-resident paths never silently download decoded images.
- [ ] Software fallback uses a direct decoder and one decoded-plane upload.
- [ ] Engine selection happens once per generation.
- [ ] Codec/API compatibility is enforced by concrete supported implementations.
- [ ] Compressed input, reference images, display output, completion work, and
      diagnostics are all bounded.
- [ ] Presentation selects the newest eligible output while retaining references.
- [ ] Normal and instrumented paths have identical timing/order behavior.
- [ ] Color, crop, orientation, and synchronization remain correct.
- [ ] Lifecycle errors are scoped and cleanup precedes replacement.
- [ ] Statistics distinguish submission from actual presentation.
- [ ] Build/test/hardware evidence and remaining limitations are reported honestly.

## 18. Primary technical references

- [Vulkan video coding specification](https://docs.vulkan.org/spec/latest/chapters/videocoding.html)
- [Vulkan video decode queue proposal](https://docs.vulkan.org/features/latest/features/proposals/VK_KHR_video_decode_queue.html)
- [Vulkan H264 decode proposal](https://docs.vulkan.org/features/latest/features/proposals/VK_KHR_video_decode_h264.html)
- [CUDA graphics interoperability](https://docs.nvidia.com/cuda/cuda-programming-guide/04-special-topics/graphics-interop.html)
- [NVDEC programming guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvdec-video-decoder-api-prog-guide/index.html)
- [GPUDirect RDMA requirements](https://docs.nvidia.com/cuda/gpudirect-rdma/)
- [wgpu surface configuration](https://docs.rs/wgpu/latest/wgpu/type.SurfaceConfiguration.html)
- [wgpu presentation modes](https://docs.rs/wgpu/latest/wgpu/enum.PresentMode.html)
- [winit event-loop APIs](https://docs.rs/winit/latest/winit/event_loop/)
- [Candidate gpu-video library documentation](https://docs.rs/gpu-video/latest/gpu_video/)

The candidate library reference is an evaluation lead, not an adoption decision.
Its version compatibility and ownership contracts require verification during
implementation.


## 19. Implementation audit (2026-10-05)

The implemented Vulkan and direct software engines, ownership contracts, test evidence and incomplete acceptance items are recorded in [gpu-stream-engine-implementation.md](gpu-stream-engine-implementation.md). Phase F (NVDEC/VAAPI interoperability) remains unimplemented. Physical Linux GPU/WSI validation and localization of the reported two-second delay remain pending. The checklist above remains an acceptance checklist rather than a claim of runtime verification.
