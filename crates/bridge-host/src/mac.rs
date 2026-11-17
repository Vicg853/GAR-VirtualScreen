use anyhow::{Context, Result, anyhow, bail};
use apple_cf::{
    cm::CMTime,
    iosurface::{IOSurface, IOSurfaceLockOptions},
};
use bridge_core::{
    config::{Acceleration, Codec as BridgeCodec, CodecOption, H264Profile, VideoSettings},
    protocol::{MediaHeader, MediaKind, VERSION},
    telemetry::{Outcome, Stage, Telemetry, TraceToken, now_us},
};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use videotoolbox::{
    compression::{CompressionSession, FrameProperties, HardwareAcceleration, ProfileLevel},
    session::Codec,
};

unsafe extern "C" {
    fn airbridge_display_position(display_id: u32, x: *mut i32, y: *mut i32) -> bool;
    fn airbridge_display_set_position(display_id: u32, x: i32, y: i32) -> i32;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn airbridge_display_create(
        width: u32,
        height: u32,
        fps: u32,
        display_id: *mut u32,
    ) -> *mut c_void;
    fn airbridge_display_release(display: *mut c_void);
    fn airbridge_display_update(display: *mut c_void, width: u32, height: u32, fps: u32) -> bool;
    fn airbridge_display_mode(
        display_id: u32,
        width: *mut u32,
        height: *mut u32,
        fps: *mut u32,
    ) -> bool;
    fn airbridge_video_stream_start(
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        vsync: bool,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *mut c_void),
        error_code: *mut i32,
    ) -> *mut c_void;
    #[cfg(not(test))]
    fn airbridge_video_stream_stop(stream: *mut c_void) -> bool;
}

pub fn capture_permission_diagnostic() -> String {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unknown ({error})"));
    let allowed = unsafe { CGPreflightScreenCaptureAccess() };
    format!(
        "AirBridge protocol v{VERSION}, CoreGraphics video-only backend. Running executable: {executable}; macOS Screen Recording preflight: {}",
        if allowed { "granted" } else { "not granted" }
    )
}

static DISPLAY_OPERATIONS: std::sync::Mutex<()> = std::sync::Mutex::new(());
static QUARANTINED: AtomicBool = AtomicBool::new(false);
pub(crate) struct VirtualDisplay(*mut c_void);

impl VirtualDisplay {
    pub(crate) fn new(settings: &VideoSettings) -> Result<(Self, u32)> {
        let _serial = DISPLAY_OPERATIONS
            .lock()
            .map_err(|_| anyhow!("Display manager unavailable"))?;
        if QUARANTINED.load(Ordering::Acquire) {
            bail!(
                "Capture cleanup was not confirmed; restart the host before creating another display"
            );
        }
        let mut id = 0;
        let ptr = unsafe {
            airbridge_display_create(settings.width, settings.height, settings.fps, &mut id)
        };
        if ptr.is_null() || id == 0 {
            bail!(
                "macOS could not create a virtual display. This macOS release may not support the private display API."
            );
        }
        Ok((Self(ptr), id))
    }
}

impl VirtualDisplay {
    pub(crate) fn update(&self, settings: &VideoSettings) -> Result<()> {
        let _serial = DISPLAY_OPERATIONS
            .lock()
            .map_err(|_| anyhow!("Display manager unavailable"))?;
        if QUARANTINED.load(Ordering::Acquire) {
            bail!("Capture cleanup unconfirmed; restart the host before reconfiguring");
        }
        if !unsafe {
            airbridge_display_update(self.0, settings.width, settings.height, settings.fps)
        } {
            bail!("macOS could not update virtual display settings");
        }
        Ok(())
    }
}
pub(crate) fn quarantine_capture() {
    QUARANTINED.store(true, Ordering::Release);
}

pub(crate) fn capture_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let Ok(_serial) = DISPLAY_OPERATIONS.lock() else {
            quarantine_capture();
            return;
        };
        if QUARANTINED.load(Ordering::Acquire) {
            eprintln!("[host] Retaining display because native capture callbacks did not stop");
            return;
        }
        unsafe {
            airbridge_display_release(self.0);
        }
    }
}

enum Captured {
    Video {
        surface: Arc<CaptureSurface>,
        timestamp_us: u64,
        sequence: u64,
        trace: Option<TraceToken>,
        queued_us: u64,
    },
}

// The retain owns the object; the use count prevents CoreGraphics from
// recycling its pixels while queued or while VideoToolbox is reading them.
struct CaptureSurface(IOSurface);

impl Drop for CaptureSurface {
    fn drop(&mut self) {
        self.0.decrement_use_count();
    }
}

type LatestCapture = Arc<Mutex<Option<Captured>>>;

struct FrameContext {
    output: LatestCapture,
    consumer: std::thread::Thread,
    started: Instant,
    dropped: Arc<AtomicU64>,
    sequence: Arc<AtomicU64>,
    telemetry: Arc<Telemetry>,
}

extern "C" fn frame_callback<const DEBUG: bool>(context: *mut c_void, surface: *mut c_void) {
    // No unwind may cross the Objective-C callback boundary.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let context = unsafe { &*(context.cast::<FrameContext>()) };
        let Some(surface) = (unsafe { IOSurface::from_raw_borrowed(surface) }) else {
            return;
        };
        surface.increment_use_count();
        let surface = Arc::new(CaptureSurface(surface));
        let Ok(mut slot) = context.output.lock() else {
            return;
        };
        let sequence = context.sequence.fetch_add(1, Ordering::Relaxed);
        let trace = if DEBUG {
            let capture_us = now_us();
            context.telemetry.sample(capture_us).then(|| TraceToken {
                sequence,
                capture_us,
                offset_us: 0,
                uncertainty_us: None,
                telemetry: context.telemetry.clone(),
            })
        } else {
            None
        };
        let packet = Captured::Video {
            surface,
            timestamp_us: context.started.elapsed().as_micros() as u64,
            sequence,
            trace: trace.clone(),
            queued_us: if DEBUG { now_us() } else { 0 },
        };
        if DEBUG {
            if let Some(t) = &trace {
                t.mark(
                    Stage::Capture,
                    t.capture_us,
                    Some(now_us()),
                    Outcome::Complete,
                    "Callback detected updated display pixels; source compositor time unavailable",
                );
                t.mark(Stage::CaptureQueue, now_us(), None, Outcome::Pending, "");
            }
        }
        let displaced = slot.replace(packet);
        drop(slot);
        context.consumer.unpark();
        if let Some(Captured::Video { trace, .. }) = displaced {
            if DEBUG {
                if let Some(t) = &trace {
                    t.mark(
                        Stage::CaptureQueue,
                        t.capture_us,
                        Some(now_us()),
                        Outcome::Dropped,
                        "Replaced by newer capture",
                    );
                }
            }
            context.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }));
}

struct VideoStream {
    ptr: *mut c_void,
    context: *mut FrameContext,
}

impl VideoStream {
    fn start<const DEBUG: bool>(
        display_id: u32,
        settings: &VideoSettings,
        output: LatestCapture,
        started: Instant,
        sequence: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self> {
        let context = Box::into_raw(Box::new(FrameContext {
            output,
            consumer: std::thread::current(),
            started,
            dropped,
            sequence,
            telemetry,
        }));
        let mut error_code = 0;
        let ptr = unsafe {
            airbridge_video_stream_start(
                display_id,
                settings.width,
                settings.height,
                settings.fps,
                settings.vsync,
                context.cast(),
                frame_callback::<DEBUG>,
                &mut error_code,
            )
        };
        if ptr.is_null() {
            unsafe { drop(Box::from_raw(context)) };
            if error_code == -2 {
                bail!("CoreGraphics capture API symbols unavailable on this macOS version");
            }
            bail!(
                "CoreGraphics stream creation returned NULL (wrapper code {error_code}, display {display_id}, permission preflight {}). No capture producer was created.",
                if capture_allowed() {
                    "granted"
                } else {
                    "denied"
                }
            );
        }
        let mut stream = Self { ptr, context };
        if error_code != 0 {
            let cleanup = stream.stop();
            bail!("CoreGraphics stream start failed (CGError {error_code}); cleanup: {cleanup:?}");
        }
        Ok(stream)
    }

    fn stop(&mut self) -> Result<()> {
        if self.ptr.is_null() {
            return Ok(());
        }
        let stopped = unsafe { airbridge_video_stream_stop(self.ptr) };
        if stopped {
            self.ptr = std::ptr::null_mut();
            unsafe { drop(Box::from_raw(self.context)) };
            self.context = std::ptr::null_mut();
            Ok(())
        } else {
            QUARANTINED.store(true, Ordering::Release);
            // Keep the entire native producer and callback data alive rather than risk a callback using
            // freed memory if macOS never confirmed the stopped event.
            bail!("CoreGraphics did not confirm that video callbacks stopped")
        }
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("[host] {error}");
        }
    }
}

pub struct EncodedPacket {
    pub header: MediaHeader,
    pub bytes: Vec<u8>,
    pub trace: Option<TraceToken>,
    pub queued_us: u64,
}

pub fn available_codecs() -> Vec<CodecOption> {
    // VideoToolbox may select a software encoder when hardware is unavailable.
    // The explicit software option allows users to override that preference.
    [Acceleration::Hardware, Acceleration::Software]
        .into_iter()
        .map(|acceleration| CodecOption {
            codec: BridgeCodec::H264,
            acceleration,
        })
        .collect()
}

fn copy_bgra(source: &IOSurface, destination: &IOSurface) -> Result<()> {
    let source_stride = source.bytes_per_row();
    let destination_stride = destination.bytes_per_row();
    let width = destination.width() * 4;
    let height = destination.height();
    let input = source
        .lock(IOSurfaceLockOptions::READ_ONLY)
        .map_err(|code| anyhow!("Capture IOSurface lock error {code}"))?;
    let mut output = destination
        .lock(IOSurfaceLockOptions::NONE)
        .map_err(|code| anyhow!("Encoder IOSurface lock error {code}"))?;
    let input = unsafe { input.as_slice() }.context("Capture surface cannot be mapped")?;
    let output = unsafe { output.as_slice_mut() }.context("Encoder surface cannot be mapped")?;
    for row in 0..height {
        let src = row * source_stride;
        let dst = row * destination_stride;
        let bytes = input
            .get(src..src + width)
            .context("Capture row truncated")?;
        output
            .get_mut(dst..dst + width)
            .context("Encoder row truncated")?
            .copy_from_slice(bytes);
    }
    Ok(())
}

pub(crate) fn capture_display<const DEBUG: bool>(
    display_id: u32,
    screen_id: u32,
    generation: u64,
    settings: VideoSettings,
    option: CodecOption,
    output: mpsc::Sender<EncodedPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    telemetry: Arc<Telemetry>,
    keyframe_request: Arc<AtomicBool>,
    progress: impl Fn(&str),
) -> Result<()> {
    if QUARANTINED.load(Ordering::Acquire) {
        bail!("Capture cleanup unconfirmed; restart host before retrying");
    }
    if option.codec != BridgeCodec::H264 {
        bail!("Requested encoder unavailable");
    }
    settings.validate()?;
    if !capture_allowed() {
        bail!("Screen Recording permission not granted for this executable");
    }
    let captured = LatestCapture::default();
    let sequence_counter = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    progress("Preparing VideoToolbox encoder…");
    let mut encoder_builder =
        CompressionSession::builder(settings.width as i32, settings.height as i32, Codec::H264)
            .with_real_time(true)
            .with_allow_frame_reordering(false)
            .with_hardware_acceleration(match option.acceleration {
                Acceleration::Hardware => HardwareAcceleration::Preferred,
                Acceleration::Software => HardwareAcceleration::Disabled,
            })
            .with_average_bit_rate((settings.bitrate_kbps * 1000) as i32)
            .with_expected_frame_rate(settings.fps as f64)
            .with_max_keyframe_interval(settings.keyframe_interval as i32)
            .with_profile_level(match settings.h264_profile {
                H264Profile::Baseline => ProfileLevel::H264BaselineAutoLevel,
                H264Profile::Main => ProfileLevel::H264MainAutoLevel,
                H264Profile::High => ProfileLevel::H264HighAutoLevel,
            });
    if let Some(quality) = settings.h264_quality() {
        encoder_builder = encoder_builder.with_quality(f32::from(quality) / 100.0);
    }
    if settings.low_latency_rate_control {
        encoder_builder = encoder_builder.with_low_latency_rate_control(true);
    }
    // Keep the most recently submitted lease alive until after the encoder
    // is invalidated, including when completion returns an error.
    let mut latest: Option<Arc<CaptureSurface>> = None;
    let mut copy_surface = None;
    let encoder = encoder_builder.build()?;
    let mut direct = std::env::var_os("AIRBRIDGE_CAPTURE_COPY").is_none();
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    progress(&format!(
        "Starting CoreGraphics video-only stream on display {display_id}…"
    ));
    let mut stream = VideoStream::start::<DEBUG>(
        display_id,
        &settings,
        captured.clone(),
        started,
        sequence_counter.clone(),
        dropped.clone(),
        telemetry,
    )?;
    progress("Video-only stream started; waiting for the first frame…");
    let capture_started = Instant::now();
    let mut received_first_frame = false;
    let mut permission_check = Instant::now();
    let mut last_keyframe = None::<Instant>;
    let mut last_force = None::<Instant>;
    let mut last_timestamp = 0u64;

    while !stop.load(Ordering::Relaxed) {
        if permission_check.elapsed() >= Duration::from_secs(1) {
            permission_check = Instant::now();
            if !capture_allowed() {
                stream.stop()?;
                bail!("Screen Recording permission was revoked during capture");
            }
        }

        let can_force = last_keyframe
            .into_iter()
            .chain(last_force)
            .all(|at| at.elapsed() >= Duration::from_millis(250));
        let packet = {
            let mut slot = captured
                .lock()
                .map_err(|_| anyhow!("Capture slot unavailable"))?;
            slot.take().or_else(|| {
                let surface = latest.as_ref()?;
                (can_force && keyframe_request.load(Ordering::Acquire)).then(|| Captured::Video {
                    surface: surface.clone(),
                    timestamp_us: started.elapsed().as_micros() as u64,
                    sequence: sequence_counter.fetch_add(1, Ordering::Relaxed),
                    trace: None,
                    queued_us: 0,
                })
            })
        };
        let packet = match packet {
            Some(packet) => packet,
            None => {
                if !received_first_frame && capture_started.elapsed() > Duration::from_secs(10) {
                    progress(
                        "Video capture delivered no frame within 10 seconds; stopping capture…",
                    );
                    bail!(
                        "CoreGraphics delivered no initial video frame within 10 seconds for virtual display {display_id}"
                    );
                }
                // Fresh capture wakes immediately; idle keyframe requests are
                // checked on the timeout without polling hundreds of times/sec.
                std::thread::park_timeout(Duration::from_millis(20));
                continue;
            }
        };
        if !received_first_frame {
            received_first_frame = true;
            progress("First video frame captured; encoding with VideoToolbox…");
        }
        let (header, bytes, trace) = match packet {
            Captured::Video {
                surface,
                timestamp_us,
                sequence,
                trace,
                queued_us,
            } => {
                let force = can_force && keyframe_request.swap(false, Ordering::AcqRel);
                if force {
                    last_force = Some(Instant::now());
                }
                if surface.0.width() != settings.width as usize
                    || surface.0.height() != settings.height as usize
                    || surface.0.pixel_format() != u32::from_be_bytes(*b"BGRA")
                {
                    bail!("CoreGraphics capture surface has an unexpected format or size");
                }
                let timestamp_us = timestamp_us.max(last_timestamp.saturating_add(1));
                last_timestamp = timestamp_us;
                latest = Some(surface.clone());
                let encode_start = if DEBUG { now_us() } else { 0 };
                if DEBUG {
                    if let Some(t) = &trace {
                        t.mark(
                            Stage::CaptureQueue,
                            queued_us,
                            Some(encode_start),
                            Outcome::Complete,
                            "",
                        );
                        t.mark(Stage::Encode, encode_start, None, Outcome::Pending, "");
                    }
                }
                let pts = CMTime::new(timestamp_us as i64, 1_000_000);
                let properties = || FrameProperties::new().with_force_key_frame(force);
                let result = if direct {
                    match encoder.encode_with_properties(&surface.0, pts, properties()) {
                        Err(videotoolbox::VTError::PixelBufferCreateFailed(_)) => {
                            direct = false;
                            progress(
                                "Capture surface cannot be wrapped; using the compatible copy path",
                            );
                            None
                        }
                        result => Some(result),
                    }
                } else {
                    None
                };
                let result = match result {
                    Some(result) => result,
                    None => {
                        if copy_surface.is_none() {
                            copy_surface = IOSurface::create(
                                settings.width as usize,
                                settings.height as usize,
                                u32::from_be_bytes(*b"BGRA"),
                                4,
                            );
                        }
                        let copy = copy_surface
                            .as_ref()
                            .context("Cannot allocate encoder surface")?;
                        copy_bgra(&surface.0, copy)?;
                        encoder.encode_with_properties(copy, pts, properties())
                    }
                };
                let encoded = match result {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        if DEBUG {
                            if let Some(t) = &trace {
                                t.mark(
                                    Stage::Encode,
                                    encode_start,
                                    Some(now_us()),
                                    Outcome::Failed,
                                    &error.to_string(),
                                );
                            }
                        }
                        return Err(error.into());
                    }
                };
                if encoded.data.is_empty() {
                    if force {
                        keyframe_request.store(true, Ordering::Release);
                    }
                    if DEBUG {
                        if let Some(t) = &trace {
                            t.mark(
                                Stage::Encode,
                                encode_start,
                                Some(now_us()),
                                Outcome::Dropped,
                                "Encoder returned no frame",
                            );
                        }
                    }
                    continue;
                }
                let keyframe = encoded
                    .cm_sample_buffer()
                    .is_some_and(|sample| sample.is_sync_sample());
                if force && !keyframe {
                    keyframe_request.store(true, Ordering::Release);
                }
                let mut annex_b = Vec::with_capacity(encoded.data.len() + 128);
                if keyframe {
                    last_keyframe = Some(Instant::now());
                    if let Some(format) = encoded
                        .cm_sample_buffer()
                        .and_then(|sample| sample.format_description())
                    {
                        if let Ok(sets) = format.video_parameter_sets() {
                            for set in sets.parameter_sets {
                                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                                annex_b.extend_from_slice(&set);
                            }
                        }
                    }
                }
                let mut offset = 0;
                while offset + 4 <= encoded.data.len() {
                    let size =
                        u32::from_be_bytes(encoded.data[offset..offset + 4].try_into()?) as usize;
                    offset += 4;
                    if offset + size > encoded.data.len() {
                        bail!("VideoToolbox emitted an invalid H.264 frame");
                    }
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(&encoded.data[offset..offset + size]);
                    offset += size;
                }
                if DEBUG {
                    if let Some(t) = &trace {
                        t.mark(
                            Stage::Encode,
                            encode_start,
                            Some(now_us()),
                            Outcome::Complete,
                            "",
                        );
                        t.mark(Stage::SendQueue, now_us(), None, Outcome::Pending, "");
                    }
                }
                (
                    MediaHeader {
                        screen_id,
                        generation,
                        kind: MediaKind::Video,
                        sequence,
                        timestamp_us,
                        keyframe,
                        width: settings.width,
                        height: settings.height,
                        sample_rate: 0,
                        channels: 0,
                    },
                    annex_b,
                    trace,
                )
            }
        };
        let token = if DEBUG { trace.clone() } else { None };
        if output
            .blocking_send(EncodedPacket {
                header,
                bytes,
                trace,
                queued_us: if DEBUG { now_us() } else { 0 },
            })
            .is_err()
        {
            if DEBUG {
                if let Some(t) = token {
                    t.mark(
                        Stage::SendQueue,
                        now_us(),
                        Some(now_us()),
                        Outcome::Cancelled,
                        "Sender stopped",
                    );
                }
            }
            break;
        }
    }
    stream.stop()?;
    Ok(())
}

pub(crate) fn request_capture_access() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

#[cfg(test)]
static MOCK_CALLBACKS_STOPPED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
unsafe fn airbridge_video_stream_stop(_stream: *mut c_void) -> bool {
    MOCK_CALLBACKS_STOPPED.load(Ordering::Acquire)
}
#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn latest_capture_releases_displaced_surface_and_leases_the_current_one() {
        let first = IOSurface::create(64, 64, u32::from_be_bytes(*b"BGRA"), 4).unwrap();
        let second = IOSurface::create(64, 64, u32::from_be_bytes(*b"BGRA"), 4).unwrap();
        let mut context = FrameContext {
            output: LatestCapture::default(),
            consumer: std::thread::current(),
            started: Instant::now(),
            dropped: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
            telemetry: Telemetry::new(),
        };
        let ptr = std::ptr::from_mut(&mut context).cast();
        frame_callback::<false>(ptr, first.as_ptr());
        assert!(first.is_in_use());
        frame_callback::<false>(ptr, second.as_ptr());
        assert!(!first.is_in_use());
        assert!(second.is_in_use());
        assert_eq!(context.dropped.load(Ordering::Relaxed), 1);
        let capture = context.output.lock().unwrap().take();
        assert!(matches!(
            &capture,
            Some(Captured::Video { sequence: 1, .. })
        ));
        drop(capture);
        assert!(!second.is_in_use());
    }

    #[test]
    fn callback_context_is_retained_until_native_quiescence_is_confirmed() {
        let context = Box::into_raw(Box::new(FrameContext {
            output: LatestCapture::default(),
            consumer: std::thread::current(),
            started: Instant::now(),
            dropped: Arc::new(AtomicU64::new(0)),
            sequence: Arc::new(AtomicU64::new(0)),
            telemetry: Telemetry::new(),
        }));
        let mut lease = VideoStream {
            ptr: std::ptr::dangling_mut::<u8>().cast(),
            context,
        };
        MOCK_CALLBACKS_STOPPED.store(false, Ordering::Release);
        assert!(lease.stop().is_err());
        assert!(!lease.ptr.is_null());
        assert_eq!(lease.context, context);
        assert!(QUARANTINED.load(Ordering::Acquire));
        MOCK_CALLBACKS_STOPPED.store(true, Ordering::Release);
        lease.stop().unwrap();
        assert!(lease.ptr.is_null());
        assert!(lease.context.is_null());
        lease.stop().unwrap(); // duplicate cleanup has no callback/free
    }
}

/// The resolution and refresh rate macOS currently drives a virtual display
/// with. Differs from the screen's settings after a switch in System Settings.
pub(crate) fn display_mode(display_id: u32) -> Option<(u32, u32, u32)> {
    let (mut width, mut height, mut fps) = (0, 0, 0);
    unsafe { airbridge_display_mode(display_id, &mut width, &mut height, &mut fps) }
        .then_some((width, height, fps))
}
pub(crate) fn display_position(display_id: u32) -> Option<bridge_core::sessions::DisplayPosition> {
    let (mut x, mut y) = (0, 0);
    if unsafe { airbridge_display_position(display_id, &mut x, &mut y) } {
        Some(bridge_core::sessions::DisplayPosition { x, y })
    } else {
        None
    }
}
pub(crate) fn restore_display_position(
    display_id: u32,
    position: bridge_core::sessions::DisplayPosition,
) -> Result<()> {
    let code = unsafe { airbridge_display_set_position(display_id, position.x, position.y) };
    if code != 0 {
        bail!("CoreGraphics display arrangement error {code}");
    }
    Ok(())
}
