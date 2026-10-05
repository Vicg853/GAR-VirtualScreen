# Screen ownership, multi-viewer sharing and failure containment plan

Date: 2026-10-04
Status: **Implemented and source-audited after user approval; automated checks completed. Real macOS capture/TCC and Linux GPU runtime verification remain with the user.**

The original investigation below is retained for comparison. The user approved implementation with “Attack this plan then please”. No UI was launched or visually inspected. No VM, container, dependency installation or external builder was used.

## 1. Required product model

### Host

- A host can create multiple screen entries. **Each host screen is one shared virtual display with its own PIN and zero or more viewers.** It is not a display created separately for each viewer.
- Creating a screen generates its PIN. The PIN appears only inside that screen's details page, never on the main widget list.
- Details contain screen settings, codec options, logs/stats and, at the bottom, the connected viewer list. Each viewer has its own status and statistics selection.
- Removing/disconnecting one viewer leaves the screen and other viewers intact. Removing the host screen notifies and detaches all its viewers, stops its pipeline and releases its virtual display.
- Screen entries, PINs and prepared displays survive ordinary viewer disconnection. They are not owned by a viewer session.

### Viewer

- Add creates a **local disconnected screen entry**, not a request to create an extra display on an already connected host.
- Its details page contains discovery/manual address, the selected host screen, PIN input and Connect. **One local viewer screen attaches to exactly one host screen on one host.**
- Different local screens can independently connect to different hosts, or explicitly select different screens on the same host.
- Config, connection state, authentication input, logs, statistics, clock estimates and native window belong to that local screen/session. No app-wide connection/PIN gate.
- Back/List keeps working streams alive and shows only list actions. An error keeps the entry with its cause and a manual Retry/Edit/Remove flow; it does not silently recreate native resources.
- Closing a viewer video window detaches that viewer screen; it never deletes the shared screen on the host. Removing a local entry detaches and removes only that entry.

## 2. Investigation findings

| Area | Current code | Gap / implication |
| --- | --- | --- |
| Host ownership | `bridge-host/src/main.rs`: global `State.pin`, `viewer_name`, `session_commands`; `run_server` awaits `handle_connection` before accepting the next session | App-wide authentication and serialized single-viewer handling. |
| Host capture ownership | `run_screen_stream_impl` starts `mac::capture` for a connection; `mac::capture` owns the virtual display | A viewer session currently owns capture/display creation. Simply accepting sessions concurrently would create duplicate displays and conflicting state. |
| Host virtual displays | `native/virtual_display.m` hard-codes descriptor serial number 1 | Multiple prepared displays need distinct native identities and serialized creation/update/removal. |
| Viewer ownership | Global `State.pin`, address, connected flags and clock; one `connect` loop handles multiple remote screens | Does not support independent local screen-to-host sessions; IDs from different hosts can collide. |
| Authentication | `security.rs` performs SPAKE2 PIN proof bound to TLS before the control Hello; Hello has no target screen | Target screen must be selected and bound to authentication before admission; wrong screen PIN must never authenticate a different screen. |
| Discovery | `discovery.rs` announces host with a fixed `version=1` TXT value | Discovery must advertise current protocol and selectable nonsecret screen identities; no PIN publication. |
| Reported capture failure | Log says preflight `not granted`; native wrapper assigns `-1` when stream creation returns NULL | Permission is a concrete suspected blocker; `-1` here does not prove a specific CoreGraphics error or macOS compatibility failure. Preflight currently only logs and does not gate startup. |
| Failure propagation | Host sends generation-scoped Failed status; viewer `end_client_lane` cancels and posts Close | There is an error path, but Close has no acknowledgement and UI terminal state is set before cleanup completion is known. Cross-channel FIN/status races remain important. |
| Generic host errors | Viewer `HostMessage::Error` handling mostly updates UI/request state; only video Failed/Stopped has the dedicated lane cleanup path | Typed operation rejection versus terminal source/session errors must remove this ambiguity. |
| Native cleanup | Decoder join/release barriers were added; same process still contains GPUI, codec and GPU calls | Those Rust/mock tests do not prove Linux driver teardown safety. A native SIGSEGV bypasses Rust error handling/unwinding and kills the app. |
| Capture FFI | Raw callback context is freed on start failure; stop waits for Stopped but then releases the stream even if confirmation failed | Audit all failure paths for callbacks already queued/in flight. Retaining only Rust context is insufficient evidence that the entire native producer is quiescent. |
| Timeout handling | Host stop timeout aborts an async wrapper around native blocking capture | Aborting a Tokio task does not prove that a native blocking operation stopped; replacement cannot use an async cancellation as a resource-release acknowledgement. |

These are findings from code and supplied logs. The exact instruction causing the Linux segfault is **not identified without a core-dump backtrace**. The plan fixes ownership/propagation gaps and contains native faults rather than claiming a particular unobserved crash cause.

## 3. Architecture decisions for review

### Shared host screen service

Introduce a host screen manager and one serialized actor per screen. The actor owns its PIN, settings revision, capture generation, virtual-display lease, capture/encoder tasks and subscriber registry. Connections are independently supervised and authenticated; they own subscriber lanes, not screens.

Host Add prepares the display on the correct macOS execution context, with a unique descriptor identity. Permission denial produces a visible Failed/Blocked screen record instead of an orphaned half-started pipeline. A prepared display can remain when no viewers are attached; capture/encoding is idle until needed. Explicit removal releases it.

Capture once and encode once for the screen's canonical video configuration; fan out immutable encoded packet references to bounded per-viewer send queues. A slow/broken viewer cannot block capture, control, or another viewer. Queue overflow must be codec-aware: never resume a dependent H.264 chain after arbitrary packet drops; skip until a fresh keyframe or request one, with visible drop/resync counters. Cap screens, connections, queues and retained bytes.

Start with the existing H.264 implementation. Viewer codec capability negotiation must be compatible with the active shared encoding; reject an incompatible subscriber clearly. Adding a viewer must not silently change another viewer's codec. Additional codec-specific encoder branches are outside this correction.

Host screen settings are authoritative. A viewer setting change is a request affecting the shared source: label that impact in the viewer and broadcast accepted settings/revision/generation changes to all subscribers. Window scaling/placement remains local. Serialize changes and coalesce pending edits to the latest desired settings; teardown must complete before replacement starts.

### Protocol and per-screen authentication

Bump the protocol version. Introduce explicit host identity, host-screen identity, viewer-local screen identity (local only), viewer-session/subscriber identity, request ID, revision and generation. Route using these scopes rather than an unqualified integer screen ID or a global connection.

Before PIN proof, exchange a bounded version/target-screen selector without starting capture or revealing protected screen data. Bind the screen identity and protocol version into SPAKE2 identities/proofs as well as the TLS exporter. Screen absence, invalid version, incorrect PIN, auth timeout and removal during authentication return bounded typed rejection messages and close only that attempted session. Rate-limit failed attempts without serializing all viewers.

Use typed failure records with code, origin, scope, request/generation, retryability and safe human message. Distinguish operation rejection from fatal media failure. Add Starting, WaitingForFirstFrame, Ready, Restarting, Stopping, Stopped/Failed and cleanup-complete acknowledgements. Repeated terminal events are idempotent; later specific failure information may enrich a generic FIN message, but cannot restart cleanup or overwrite an unrelated generation.

Each viewer screen has its own control session and video/diagnostic lanes. Keep audio/input/output lane types reserved and independent; no audio activation. Control/diagnostic queues stay bounded, with failure of one peer's writer isolated from other peers.

Discovery exposes host/screen IDs, friendly names, current protocol and availability only. PINs never appear in mDNS or logs. Preserve manual address + screen selection when multicast discovery is unavailable.

### Viewer native fault containment

Move the existing native video backend into a supervised **per-screen worker mode of the viewer executable**. This is a local child process, not a VM/container or FFmpeg. It owns its winit window, decoder, GPU device, importer, frame queues and teardown on its own threads; GPUI controls and host control sessions remain in the parent.

The parent forwards compressed packets over versioned, bounded local IPC, with screen/session/generation/worker-instance checks. No decoded pixels cross IPC: decode-to-GPU remains within the worker. Limit packet size, queued packets/bytes, worker count and telemetry queues; measure IPC queue latency in instrumented mode. Start with a bounded Unix socket plus acknowledgements, without introducing unbounded shared-memory caches. Preserve separate compiled normal/instrumented media paths.

Initialize native decoding only after valid current-generation video/keyframe arrives. A host error before the first frame therefore does not create a decoder unnecessarily. Window creation/Ready transitions must be explicit, without relying on the race order of different QUIC streams.

Normal stop closes packet admission, cancels receiving tasks, drains/completes submitted GPU work, releases importer/presented/queued frames, releases the decoder barrier, joins the worker thread and closes the native window. The parent waits for Stopped and process exit before marking cleanup complete or reusing a worker instance.

If the worker crashes (including SIGSEGV) or hangs, the parent detects/reaps it, closes its IPC and media consumer, marks the local screen Failed, preserves the host cause when present, and keeps controls/other screens alive. A bounded shutdown deadline may terminate **the isolated process**, never free live native resources inside the controls process. No automatic retry loop. Lifecycle logs include worker exit signal and cleanup result without routine packet logs.

### macOS capture failures and callback ownership

Check current capture permission before starting capture; provide a specific UI error and an explicit user action to request/access settings. Do not keep trying to create capture streams after known denial. Keep the authenticated control session usable and forward the screen's failure to its viewers.

Return distinct native startup phases: unavailable API symbol, virtual-display creation/application failure, stream-create NULL, stream-start CGError, first-frame timeout and callback-stop failure. Include executable/bundle identity and backend in diagnostic details. The current packaging/signing behavior must be described accurately; a script cannot grant TCC permission or promise stable consent after ad hoc rebuilds.

Keep native capture object, callback context, dispatch queue and virtual display alive until callbacks are confirmed stopped and the callback queue is drained. Audit partially successful creation/start and native exception paths. A timeout must retain/quarantine the complete producer ownership and block conflicting restart, rather than treat the resources as released. Bound unresolved cleanup and expose it in UI; do not accumulate indefinite hidden contexts.

Keep the video-only CoreGraphics path for the first fix. If it still fails with permission actually granted, collect typed native phase information before proposing a backend change. Do not reintroduce audio or an unmeasured ScreenCaptureKit startup delay as an assumed remedy.

## 4. Lifecycle acceptance matrix

These cases are implemented in the actor/session/supervisor paths and were audited against the code. Automated coverage is listed below; native platform behavior is not claimed verified by every row.

| Event | Host behavior | Viewer behavior / final UI |
| --- | --- | --- |
| Host Add screen | Allocate independent record/PIN; prepare display or retain actionable failure | Discovery sees available screen; existing streams unaffected |
| Viewer Add local screen | No remote display creation | Create disconnected details page with target/PIN input |
| Connect/auth success | Admit only selected screen subscriber | Attach one host screen; show Starting, then Ready after usable video |
| Wrong PIN/version/missing screen/auth timeout | Reject attempted subscriber; no capture mutation | Close attempted session; retain entry and typed error; no decoder/window leak |
| Screen removed during handshake | Reject stale admission | Cancel attempt, show unavailable; no late worker creation |
| Second viewer joins same screen | Reuse same virtual display/capture/encoding; add bounded sender | New viewer starts from keyframe; existing viewer continues |
| Viewer attaches to another host in another local entry | Independent host sessions | Separate IDs/clocks/settings/logs; no global state overwrite |
| Permission denial/revocation | Typed screen failure; stop affected media safely; notify all subscribers | Cancel media and native consumer; close window; preserve permission cause |
| Display/API/encoder/capture startup failure | Unwind partial startup in dependency order; notify subscribers | Remain responsive, clean up any worker, show Failed with Retry/Edit |
| First frame never arrives | Fail that generation after bounded startup deadline | Stop waiting/consumer; show first-frame timeout, not generic connection failure |
| Live config edit | Serialize shared restart; notify all viewers; apply only latest valid revision | Stop old generation, acknowledge cleanup, accept replacement; late frames ignored |
| Stats opens/closes / debug mode change | Track subscriptions per viewer; broadcast source generation changes if needed | Restart affected native mode safely; preserve independent subscriptions |
| Hardware decoder error/fallback | Keep host control/source alive | Release old decoder dependencies before fallback; terminal failure closes worker |
| GPU/import/presentation error or device loss | Detach only failed subscriber if necessary | Close consumer in order; show backend failure; controls survive |
| Native worker panic/segfault/abnormal exit | Receive detach/status, preserve other subscribers | Parent reaps worker; closes lane/IPC; records signal and shows Failed |
| Native worker hang/stop deadline | Other viewers unaffected | Terminate/reap isolated worker; show cleanup failure; no in-process forced free |
| Slow viewer/queue overflow | Bounded independent queue, keyframe resync or peer detach | Show drop/resync state; no unbounded memory growth |
| Media FIN/reset before/after failure status | Typed terminal cause remains available on control | Idempotent cleanup; specific error enriches generic FIN cause |
| Local video window close | Remove only that subscriber | Stop local worker/consumer, retain disconnected entry |
| Viewer disconnect/remove local entry | Detach only selected subscriber; preserve shared display | Close owned session/worker; retain or remove local record as requested |
| Host disconnects one viewer | Remove selected subscriber only | Cleanup its entry; other local screens/host viewers continue |
| Last viewer disconnects | Idle capture/encoder safely; retain screen/PIN/display | Entry becomes disconnected; manual reconnect possible |
| Host removes screen | Notify all viewers, stop fanout/capture, release display, withdraw discovery | Close attached workers; show remote screen removed; no automatic re-add |
| Host shutdown/network loss/QUIC timeout | Supervise sessions, orderly stop on normal shutdown | Clean up affected entries independently; preserve endpoint/error details |
| Viewer application quit | Detach subscribers | Stop all receive tasks/workers, reap children, release discovery/network resources |
| Duplicate/stale statuses, acks, packets or late exits | Ignore wrong session/generation; removal cannot resurrect screen | Reject obsolete worker-instance events; cleanup runs once |
| Malformed/oversized/wrong-direction messages | Reject only offending session/lane as scoped | Bound allocation, typed failure and safe cleanup |
| Discovery disappears or changes | Update catalog; explicit removal distinct from mDNS loss | Discovery loss alone does not kill a healthy connected stream |

Future audio/input/output failures follow scoped optional-lane semantics and must not terminate video/control unless the connection itself fails. Their APIs remain disabled in this implementation.

## 5. UI interaction acceptance map

- Host List: Add, select/highlight a screen, screen state summary; no PIN or selected-screen actions leaking into the list.
- Host Details: Back, screen PIN, compact video/codec settings, presets, logs, Stats, Remove screen; connected viewers at bottom with per-viewer status, select stats and Disconnect.
- Viewer List: Add local entry, select/highlight entry, endpoint/state summary; no global PIN/Connect widget.
- Viewer Disconnected Details: discovered host/screen selection, manual target entry, PIN input, Connect, Back, Remove entry.
- Viewer Connecting Details: phase feedback, Cancel, Back; no duplicate Connect.
- Viewer Active Details: local controls, shared-source config requests with clear impact, logs/stats, Show video, Disconnect, Back, Remove entry.
- Viewer Failed Details: retained target/config and specific cause, cleanup progress/result, Retry after cleanup completion, Edit target/PIN, Remove. Retry creates a new session/worker instance.
- Pending start/restart/remove/stop: disable conflicting actions; list selection and unrelated screens remain usable.
- List/details transition preserves per-entry editing state; contextual menus close/hide appropriately. Window placement and controls/display separation remain as requested.

## 6. Implementation sequence after review

1. [x] Shared identity, typed errors, lifecycle state transitions and versioned auth/control/worker IPC definitions, with serialization/state-machine tests.
2. [x] Host screen actor ownership; per-screen PIN/discovery; concurrent connection supervision; shared capture and bounded fanout; subscriber detach semantics.
3. [x] Permission gate and native startup/stop phase reporting; audit callback/queue/display/encoder resource ownership and incomplete teardown.
4. [x] Viewer-local screen records and independent per-screen connection tasks; remove global PIN/session coupling and remote Add/Remove ownership confusion.
5. [x] Supervised native worker mode, bounded compressed IPC, lazy decoder startup, acknowledged cleanup and crash/hang containment; retain direct native decoding/rendering.
6. [x] Integrate host/viewer compact list/details interactions and per-viewer diagnostics without disrupting ordinary frame paths.
7. [x] Exercise the lifecycle matrix in automated integration tests; audit every row and list all implemented events and interactions directly in the final chat response.

## 7. Verification and delivery

- Unit tests for state transitions, scoped authentication, independent identities, stale/duplicate messages, error precedence, cleanup acknowledgements and queue bounds.
- QUIC integration tests: two viewers on one screen, independent screen sessions, wrong PIN for another screen, one viewer disconnecting, shared restart, host startup failure and FIN/status race orders.
- Worker supervisor tests: controlled startup failure, partial initialization, malformed IPC, closed/full IPC, forced child crash, hung worker and exit during restart. Inject deterministic failures; no visual automation required. Verify child reaping and other screens/parent responsiveness.
- Native mock ownership tests for callback start/stop failure, decoder/importer/frame release order, no post-stop callbacks and no replacement before cleanup completes. These do not substitute for actual GPU/driver tests.
- Regression tests for bounded memory, codec-aware slow-subscriber recovery, normal/instrumented path selection, independent stats subscriptions and UI action visibility/navigation state.
- Run formatting, workspace tests, release compilation and host packaging using existing dependencies. Attempt native Linux compilation only with available toolchain/sysroot; disclose missing dependencies. No VMs, containers or external builders.
- Do not launch UI or visually inspect. User supplies runtime/visual issues. macOS TCC and Linux real-driver validation remain separately identified.
- Update this checklist with evidence and remaining limits. Final output links the audited plan and **lists the lifecycle cases and UI interactions in chat**, marking any unverified runtime cases clearly.

## 8. Implementation evidence and audit

| Area | Implemented code / verification |
| --- | --- |
| Target auth and bounded protocol | `bridge-core/security.rs`, protocol v5, screen selector and typed selector rejection; SPAKE identities include screen/version and TLS proof. QUIC tests cover correct PIN, wrong screen PIN, absent screen and transport close. Admission caps: 64 simultaneous sessions, 10 attempts per IP per 10 seconds, 128 admission buckets. |
| Shared screen and PIN ownership | `bridge-host/service.rs`: actor-owned display/native thread, subscriber registry, immutable packet fanout. `main.rs`: PIN in details only, viewer list and peer controls. Actor integration tests attach two peers, broadcast shared config/debug restart, inject failure and remove safely. |
| Capture failure and ownership | `mac.rs`: permission gate and one-second permission revocation checks, creation/start phase messages, quiescence-dependent callback context release and quarantine. Native wrapper retains partial-start ownership and releases only after stop and callback-queue drain. Mock test proves Rust context/producer pointer are retained on failed stop, freed after confirmed stop, and duplicate stop is safe. Private display operations are serialized; identities are unique. |
| Capture deadlines | Actor waits for first usable source frame, fails startup after 15 seconds, signals stop, and quarantines an unconfirmed stop after 10 seconds. Removal/idle native join waits at most 3 seconds for a completed native thread; it never treats abort as native teardown. An unresolved producer/display stays retained and new native work is blocked until host restart. |
| Control remains independent | QUIC service integration test injects permission failure, receives typed video failure, then successfully exchanges a clock probe on the same authenticated control channel. Audio is disabled. |
| Viewer-local ownership | `bridge-viewer/main.rs`: independent local entry state, current-thread runtime and bounded command queue per entry. Root Add creates a local entry. Target/PIN/config/clocks/diagnostics belong to it. Legacy remote Add/Remove controls were removed; unsolicited additional host screens and mismatched target config are rejected. |
| Native fault containment | `linux.rs` supervises a private socket and same-executable child; `native_video.rs` owns direct decoding/rendering. Native startup is lazy at keyframe. Replacement awaits previous supervisor join/reaping. Child shutdown needs Stopped + exit. Parent contains abnormal exit and terminates/reaps a hung child after six seconds. |
| Worker IPC identity and bounds | IPC carries protocol version; socket is unguessable, mode 0600, unlinked after acceptance, and scoped to one worker instance. Parent input epochs reject stale queued data. Workers are replaced between host generations, so no socket is shared across sessions/generations. Compressed payload <=8 MiB, metadata <=64 KiB, diagnostics <=24 events/message, admission queue one packet. |
| Native supervisor tests | Supplemental macOS compilation of the exact direct decoder/renderer code, excluding only Linux winit extension calls, with fixture child processes that never open UI/GPU. Tests cover failed startup, clean acknowledgement/reaping, abnormal exit, hung stop/kill/reaping and host error before keyframe. This is not a Linux build or a reproduction of the actual SIGSEGV. |
| Frame and GPU dependency order | Existing seven release-barrier tests cover full/closed queues, fallback release failure, queued-frame release, obsolete producers and panic unwind. Actual GPU completion/importer teardown is contained inside the isolated worker. |
| Per-peer diagnostics | Screen source instrumentation is enabled while any host/viewer subscription needs it. Source observations fan out to independently scoped peer collectors; each sender has its own timings, queue/drop counters and transport data. Worker IPC clock mapping and IPC queue stage added. Normal/instrumented supervisors and media functions are separately monomorphized. |
| Error precedence and stale events | Viewer regression tests cover host cause after generic FIN, duplicate/removal handling, obsolete generation messages, late settings ack, rapid edits and shutdown. Specific host causes survive later generic native closure. Explicit retry starts a new one-screen session. Paused/failed subscribers are not reactivated by unrelated source diagnostics restarts. |
| Widget flow | List/details state and visibility tests retained. Host/list PIN separation, viewer-local Add/Remove, source impact label, connected peer selection/disconnect, bounded offline log toggle and cleanup progress/Retry gating source-audited. List navigation keeps other streams alive; custom borders/drag/resize/minimize/close retained. |
| Native build dependency | `bridge-host/build.rs` now explicitly tracks `native/virtual_display.m`; release linking exposed the previously stale native library. |

### Completed checks

- `cargo test --workspace --offline`: **47 tests passed** (18 core unit, 5 QUIC integration, 4 host, 20 viewer/resource lifecycle).
- Supplemental native supervisor/renderer check: **13 tests passed**, no UI/GPU launch.
- `cargo build --workspace --release --offline`: successful macOS host/shared-viewer build.
- `scripts/package-macos.sh` offline: built and signed `dist/AirBridge Host.app`; bundle was not opened.
- Formatting, diff whitespace and strict bundle signature checks: successful.
- Actual Linux target check attempted with the installed Rust target; blocked by **missing `x86_64-linux-gnu-gcc`**. No installation or VM/container workaround.

### Scope and verification limits

- The original Linux segfault instruction is still unidentified without a core-dump backtrace. Isolation makes a native child crash a scoped failure rather than a GPUI process crash; real GPU/driver fault behavior remains unverified here.
- Actual TCC grant, macOS private display behavior, callback timing, multi-machine networking and widget visuals were not exercised. The supplied log's `not granted` condition is now gated and reported specifically; the code cannot grant permission itself.
- A stuck in-process host native operation is quarantined, not forcibly reclaimed. Its screen retains an actionable cleanup error; host restart is necessary. A retained display is not falsely reported as successfully released.
- Worker scope is enforced by dedicated private socket/process and parent epoch, rather than repeating every identity in every local IPC packet. QUIC scope remains session + screen + generation.
- The pure `failure::Lifecycle` transition helper has unit tests; production lifecycle is enforced by serialized screen actor commands, generation guards, worker ownership and cleanup acknowledgements.
- Typed native failure codes are derived from explicit startup phase messages. Raw CoreGraphics codes remain in details; a wrapper NULL sentinel is not presented as a native CGError.
- Automated tests use mock capture and fixture processes; they do not claim all 28 lifecycle rows were exercised against actual macOS/Linux native APIs.

## Review boundary

User review was received before source changes. Implementation followed the approved shared-source, persistent-display and isolated-worker choices. The remaining runtime checks above are explicitly distinguished from implemented code and passing deterministic tests.
