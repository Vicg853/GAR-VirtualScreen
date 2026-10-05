# Viewer native resource lifecycle

Implemented 2026-10-04, following the reported segmentation fault on stream close/reconfiguration.

## Problems found in code

- Decoder threads were detached: dropping the app did not wait for native decoder destruction.
- Reset/fallback could replace a native decoder before the display had released its previous decoded frames and imported GPU surfaces.
- Application return did not wait for the native display event thread to stop.
- Old network producers could still enqueue packets into a reused decoder after a stream generation changed.

These are concrete lifetime/order gaps. Without the Linux core dump stack, none is asserted to be the exact faulting instruction.

## Close ordering

1. Stop input admission and discard pending input.
2. Drop the decoder command sender; mark the display closed.
3. Wait for submitted GPU work with the device completion API, without an arbitrary sleep.
4. Release frame importer leases and the current presented frame.
5. Drop the decoded receiver (also unblocks a full output queue or cancels a reset acknowledgement), remaining frames and renderer.
6. Release the worker's native-backend lifetime guard.
7. Join the decoder thread. Native backend destruction happens on that worker after consumers release their resources.
8. Release the window.

The guard wakes the display on worker failure and waits for consumer release on normal return, errors and Rust panic unwinding. It is declared after the native backend to enforce drop ordering. GPU cleanup errors and worker panics are recorded in lifecycle logs. A native segmentation fault cannot be caught by Rust unwinding.

## Reset and fallback ordering

The decoder sends a release-barrier event through its frame queue and waits for an acknowledgement. The event loop waits for GPU work, discards old frames and drops the frame importer before acknowledging. Only success permits native decoder replacement. The importer is recreated lazily for the new output. Within a native worker, reset/fallback uses this barrier. A host generation change now replaces the whole isolated worker only after the previous child exits and is reaped.

Input handles retain a generation. Reset advances the shared generation, invalidating old producer clones; packets carry the generation too, so late enqueued packets cannot be decoded after a newer reset. Close invalidates producers immediately.

## Lifecycle paths covered

- Local video window close or destruction.
- Screen removal, video lane termination/failure, disconnect and session cleanup (all close the same managed video app).
- Configuration restart and instrumentation restart.
- Hardware decoder failure and fallback to another decoder/software.
- Decoder startup failure, normal exit, error exit and Rust panic.
- Application quit: requests event-loop shutdown, releases all managed apps, joins decoder workers and joins the display thread before returning.

## Verification

- `cargo test --workspace --offline`: 44 passed, including seven new lifecycle tests.
- New tests cover native-owner release ordering, Rust panic ordering, disconnected consumer, full queue cancellation, explicit barrier acknowledgement, failed GPU release and obsolete producers.
- Supplemental check crate compiled the current renderer against the locked real `moq-video`/`wgpu` APIs on macOS and passed all seven lifecycle tests. Only Linux-specific Wayland/X11 event-loop builder calls were omitted. This is not Linux driver/runtime validation.
- `cargo build --workspace --release --offline`: passed on macOS.
- Formatting and whitespace checks passed.
- Temporary supplemental check crate removed after verification.
- No UI launch or visual checking. Full Linux compilation remains unverified because the existing environment lacks the GNU Linux native cross compiler/sysroot.

Native GPU/codec libraries contain FFI outside Rust's ownership guarantees. The reported fault still needs a Linux runtime check after rebuilding; if it persists, the core dump backtrace is needed to locate the native failure. The new waits intentionally do not detach threads or free resources merely because a driver takes too long: a hung native driver can therefore delay shutdown.


## Native process containment added in this correction

The Linux implementation now lives in `native_video.rs` and runs only in `--native-video-worker` mode. `linux.rs` in the controls process supervises one child per local screen, a private versioned socket, a one-packet admission queue, and bounded compressed payloads. Parent input epochs reject obsolete queued data; an IPC connection belongs to one worker instance and cannot be reused by its replacement. No decoded image crosses the socket.

Stop invalidates admission immediately. The child closes consumers in the order above, joins its event/decoder threads, sends Stopped and exits. Parent cleanup completes after exit/reaping and status-reader join. Startup errors, worker abnormal exit, write stalls, missing acknowledgements and six-second stop hangs produce a local failure. A hung child is terminated and reaped; no live native object is force-freed inside GPUI. An explicit host failure takes precedence over a subsequent generic EOF/worker teardown error.

Supervisor fixture tests launch no UI or GPU. They cover startup exit, normal acknowledgement/reaping, abnormal process exit, hung stop termination, and host failure before any keyframe. Real SIGSEGV/core-dump and Linux driver behavior were not reproduced on this macOS machine.
