# Stream diagnostics implementation plan

## Requirements and acceptance criteria

- [x] Remove routine frame/packet progress logging. Keep lifecycle and actionable errors.
- [x] Make diagnostic collection asynchronous and bounded; never wait for diagnostic delivery in media processing.
- [x] Compile normal and instrumented implementations separately using Rust monomorphization. Choose an implementation only when a stream generation starts; normal implementations compile out timing collection.
- [x] Opening a screen's Stats menu requests an instrumented generation; closing it requests normal streaming. Host and viewer subscriptions are independent so closing one menu cannot disable the other's diagnostics.
- [x] Scope all diagnostic messages by screen, generation, and viewer connection. Reject obsolete generations. Select a viewer on the host. The existing server accepts one viewer session at a time; do not imply new shared-display fanout support.
- [x] Exchange four monotonic timestamps to estimate host clock offset, RTT and uncertainty. Bound probes, reject invalid samples, periodically refresh, and use a fixed clock mapping for each traced frame.
- [x] Keep ordinary statistics separate: fixed-capacity rolling bitrate/FPS/RTT/jitter/drop/queue history, plus explicitly unavailable future audio metrics.
- [x] Detailed frame mode: one row per sampled frame, stage columns with start/duration/result, at most 20 rows. Default sampling around once a second, freeze/resume and a next-20-frames burst; limit UI updates.
- [x] Average timing mode: one row per stage with incremental averages, counts, failures and unavailable/incomplete outcomes. Keep constant-size accumulators, not frame history for calculating averages.
- [x] Track capture callback, capture queue, encoding, sender queue, QUIC write, complete reception, decoder queue/decode, GPU upload/submission/completion and presentation submission. Distinguish true presentation feedback from submission; never invent monitor scanout or partial rendering measurements.
- [x] Report dropped, cancelled, failed, pending/timed-out and unobservable stages with elapsed time and reason. Correlate decoded frames by decoder timestamp rather than assuming one packet yields one frame.
- [x] Integrate compact Stats controls below video settings in both GPUI widgets, retaining independent video windows.
- [x] Add tests for clock math, bounds, aggregation, wire framing, subscriptions/generation changes and failures. Run formatting, workspace tests and macOS compilation. Attempt Linux compilation only with existing native cross dependencies; no installations, VMs, containers, remote builders or visual checks.

## Implementation order

1. Shared diagnostic types, monotonic clock, collector and compile-time instrumentation policies.
2. Versioned diagnostic control/media framing and stream restart subscription flow.
3. Host capture/encode/transmit and viewer receive/decode/render measurement hooks.
4. Stats widgets, frame/average timing modes and sampling controls.
5. Automated checks and requirement-by-requirement audit.

## Verification record

Audited against the implementation on 2026-10-03. Checked requirements indicate implemented code and the automated evidence below; they do not claim visual or hardware runtime verification.

### Implementation evidence

| Requirement | Implementation and audit result |
| --- | --- |
| Logging | Routine sender/receiver frame progress logs removed. Lifecycle and error logging retained; console/file writes use a bounded background worker in `diagnostics.rs`. |
| Bounded asynchronous collection | `telemetry.rs` uses nonblocking `try_send`, 1,024-event queues, 20 frame rows, 60 rolling statistic samples, fixed stage accumulators and bounded reasons. Numeric metadata only; no frame buffers retained by diagnostics. Loss is counted. |
| Separate compiled paths | Host capture/callback/send and viewer receive/decode/render use `<false>` and `<true>` monomorphized implementations. Stream startup selects the variant; renderer function pointers and event-loop policy are selected on lifecycle changes. Normal media paths compile out diagnostic timing logic. |
| Subscription lifecycle | Host and viewer owners are independent. Opening/closing Stats restarts the affected generation when instrumentation changes. Obsolete generations are rejected; returning to list closes local subscriptions. |
| Protocol | Protocol version 4 has dedicated diagnostic media framing and a separate asynchronous QUIC diagnostics lane scoped by connection, screen and generation. Diagnostic sending has bounded batches and timeout; video does not await it. |
| Clock | Four monotonic timestamps estimate offset, RTT and uncertainty. Invalid probes rejected; outstanding probes bounded; estimates refreshed. A sampled frame retains its clock mapping. Cross-machine starts are unavailable until synchronized; durations remain valid. |
| General statistics | Separate rolling bitrate, FPS, RTT/variation, transport counters, drops and queue information. Sender queue occupancy measured; unavailable capture/decoder queue gauges remain explicitly unavailable. Future audio schema exists without audio collection. |
| Frame display | One row per sampled frame, stage columns with start/duration/result, maximum 20 rows, approximately 1 Hz sampling, freeze/resume and next-20 burst. UI snapshot refresh limited to 500 ms. |
| Average display | One row per stage with incremental averages of measured starts/durations, counts, failures and last outcome. Average calculation uses fixed accumulators rather than retained frame history. Bounded frame records remain for correlation/deduplication. |
| Stage coverage | Capture callback, capture queue, encode, sender queue, write, estimated transfer, receive, decode queue/decode, render queue, upload/import, GPU submit/completion callback and compositor submission. Decoder outputs correlated by timestamp; correlation state bounded. |
| Incomplete/error reporting | Pending stages expire; failures, drops, cancellation and unavailable stages carry outcomes/reasons. Frame summaries include observed elapsed time and failed/incomplete stages. |
| UI | Both configuration widgets have Stats below video settings, modes and sampling controls. Host identifies/selects its authenticated peer; video windows remain separate. |

### Automated verification

- `cargo test --workspace --offline`: **37 tests passed** (core 14, QUIC integration 4, host 6, viewer 13). Includes clock math/filtering, bounded storage/sampling, incremental aggregation, duplicate events, pending expiry/cancellation, wire framing, independent subscriptions, generation replacement and obsolete diagnostics.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
- `cargo build --workspace --release --offline`: passed on macOS.
- `CARGO_NET_OFFLINE=true bash scripts/package-macos.sh`: passed; rebuilt and ad hoc signed `dist/AirBridge Host.app`.
- `cargo check -p bridge-viewer --target x86_64-unknown-linux-gnu --offline`: **blocked by missing `x86_64-linux-gnu-gcc`** in native dependencies. No compiler installation, VM, container or remote builder used.
- Supplemental rendering API check: copied the current Linux renderer into a temporary macOS check crate using the same locked `moq-video`/`wgpu` dependencies, omitting only the Linux-only Wayland/X11 event-loop builder calls. `cargo check --offline` passed. This checks Rust rendering/decoder API compatibility, **not Linux linking, drivers or runtime behavior**. Temporary check crate removed afterward.

### Remaining limits and measurement meanings

- The host currently accepts **one viewer session at a time**. Selecting the authenticated peer is implemented; shared-display fanout to N viewers is not added by this change.
- Capture time is when the native capture callback observes the frame, not the unknowable instant of physical source-screen change.
- Transfer time is an estimate from sender write start to complete reception, including QUIC writing and receiver scheduling. Network asymmetry limits clock accuracy; uncertainty is displayed.
- GPU completion is callback observation. Compositor submission is measured separately. Actual monitor scanout and partial presentation are unavailable and labelled accordingly.
- Diagnostics are intentionally lossy and asynchronous. They can lag the displayed frame; instrumentation/restarting can affect timings. The normal generation contains no diagnostic timing checks in its media functions, but remains subject to ordinary transport and application scheduling.
- No UI was launched or visually checked. Real macOS capture, Linux decoding/driver behavior and end-to-end timing must be exercised on the user's machines.
- This implements tools to investigate latency; it does not claim that the underlying latency cause has been identified or fixed.
- Protocol version changed: rebuild **both host and viewer** before connecting.
