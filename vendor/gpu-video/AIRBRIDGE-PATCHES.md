# AirBridge vendor provenance and patches

Base: `gpu-video` 0.4.0 from crates.io, published by Software Mansion. Upstream repository: https://github.com/software-mansion/smelter. Original MIT license is preserved in `LICENSE`. Source, manifest and build script are vendored to make the native decoder changes reviewable and reproducible.

AirBridge patches:

- wgpu 30 integration, initial imported-texture state, queue index selection.
- Complete Annex-B access-unit flushing and explicit no-reordering mode.
- Asynchronous native decode/copy submission, timeline polling and status queries per submission.
- Input, session, DPB and output leases; unwind/submission guards; retirement after native and graphics completion.
- Picture-only batch admission allows trailing reference-release/metadata instructions at full capacity, with no partial submission on admission rejection.
- Four pending compressed submissions; bounded input/output recycling; 256 MiB allocator permits using actual Vulkan allocation sizes.
- Coded macroblock extents kept distinct from visible cropped output; left/top plane-copy offsets.
- Imported NV12 textures finish in shader-read layout, with a graphics timeline wait and semaphore lifetime retained until graphics completion.
- A graphics queue handle reserved separately from native queues. Hardware initialization is rejected when the required independent queue cannot be reserved. Required extensions are not silently discarded; optional encoding codec extensions follow availability. Unused descriptor indexing/video-maintenance feature requirements are removed. Dedicated transfer/compute families are preferred, with universal-family fallback subject to independent queue reservation.
- Compile-time measured and unmeasured submit methods, with separate input-upload and submit timing.
- `vulkan-typecheck` enables Vulkan source compilation on macOS for the supplemental test tool; the production viewer remains Linux specific.

Unused upstream bytes-download/encoder/transcoder APIs remain in this package but are not AirBridge engine paths. They have not received runtime validation as part of this change. Default-feature compilation is checked; optional transcoder functionality is outside the implemented viewer scope.

No Linux Vulkan driver execution has been verified on this macOS machine. Review the implementation audit and perform real-device stress tests before treating the patched native interoperability path as validated.
