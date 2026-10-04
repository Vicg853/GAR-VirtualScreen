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
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use videotoolbox::{
    compression::{CompressionSession, HardwareAcceleration, ProfileLevel},
    session::Codec,
};

unsafe extern "C" {
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
    fn airbridge_video_stream_start(
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        context: *mut c_void,
        callback: extern "C" fn(*mut c_void, *const u8, usize, usize),
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
        bytes: Vec<u8>,
        row_bytes: usize,
        timestamp_us: u64,
        sequence: u64,
        trace: Option<TraceToken>,
        queued_us: u64,
    },
}

struct FrameContext {
    output: mpsc::Sender<Captured>,
    started: Instant,
    dropped: Arc<AtomicU64>,
    sequence: AtomicU64,
    telemetry: Arc<Telemetry>,
}

extern "C" fn frame_callback<const DEBUG: bool>(
    context: *mut c_void,
    bytes: *const u8,
    len: usize,
    row_bytes: usize,
) {
    // No unwind may cross the Objective-C callback boundary.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let context = unsafe { &*(context.cast::<FrameContext>()) };
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
        let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
        let packet = Captured::Video {
            bytes: bytes.to_vec(),
            row_bytes,
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
        if context.output.try_send(packet).is_err() {
            if DEBUG {
                if let Some(t) = &trace {
                    t.mark(
                        Stage::CaptureQueue,
                        t.capture_us,
                        Some(now_us()),
                        Outcome::Dropped,
                        "Capture queue full",
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
        output: mpsc::Sender<Captured>,
        dropped: Arc<AtomicU64>,
        telemetry: Arc<Telemetry>,
    ) -> Result<Self> {
        let context = Box::into_raw(Box::new(FrameContext {
            output,
            started: Instant::now(),
            dropped,
            sequence: AtomicU64::new(0),
            telemetry,
        }));
        let mut error_code = 0;
        let ptr = unsafe {
            airbridge_video_stream_start(
                display_id,
                settings.width,
                settings.height,
                settings.fps,
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
    let (captured_tx, mut captured_rx) = mpsc::channel(4);
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
    if settings.low_latency_rate_control {
        encoder_builder = encoder_builder.with_low_latency_rate_control(true);
    }
    let encoder = encoder_builder.build()?;
    let surface = IOSurface::create(
        settings.width as usize,
        settings.height as usize,
        u32::from_be_bytes(*b"BGRA"),
        4,
    )
    .ok_or_else(|| anyhow!("Cannot allocate encoder surface"))?;
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    progress(&format!(
        "Starting CoreGraphics video-only stream on display {display_id}…"
    ));
    let mut stream = VideoStream::start::<DEBUG>(
        display_id,
        &settings,
        captured_tx,
        dropped.clone(),
        telemetry,
    )?;
    progress("Video-only stream started; waiting for the first frame…");
    let capture_started = Instant::now();
    let mut received_first_frame = false;
    let mut permission_check = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if permission_check.elapsed() >= Duration::from_secs(1) {
            permission_check = Instant::now();
            if !capture_allowed() {
                stream.stop()?;
                bail!("Screen Recording permission was revoked during capture");
            }
        }

        let packet = match captured_rx.try_recv() {
            Ok(packet) => packet,
            Err(mpsc::error::TryRecvError::Empty) => {
                if !received_first_frame && capture_started.elapsed() > Duration::from_secs(10) {
                    progress(
                        "Video capture delivered no frame within 10 seconds; stopping capture…",
                    );
                    bail!(
                        "CoreGraphics delivered no initial video frame within 10 seconds for virtual display {display_id}"
                    );
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        };
        if !received_first_frame {
            received_first_frame = true;
            progress("First video frame captured; encoding with VideoToolbox…");
        }
        let (header, bytes, trace) = match packet {
            Captured::Video {
                bytes,
                row_bytes,
                timestamp_us,
                sequence,
                trace,
                queued_us,
            } => {
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
                let mut guard = surface
                    .lock(IOSurfaceLockOptions::NONE)
                    .map_err(|code| anyhow!("IOSurface lock error {code}"))?;
                let destination = (unsafe { guard.as_slice_mut() })
                    .context("Encoder surface cannot be mapped")?;
                let destination_row = surface.bytes_per_row();
                let copy_width = (settings.width as usize) * 4;
                for row in 0..settings.height as usize {
                    let source_start = row * row_bytes;
                    let destination_start = row * destination_row;
                    if source_start + copy_width > bytes.len()
                        || destination_start + copy_width > destination.len()
                    {
                        bail!("Capture buffer has unexpected dimensions");
                    }
                    destination[destination_start..destination_start + copy_width]
                        .copy_from_slice(&bytes[source_start..source_start + copy_width]);
                }
                drop(guard);
                let encoded =
                    match encoder.encode(&surface, CMTime::new(timestamp_us as i64, 1_000_000)) {
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
                let mut annex_b = Vec::with_capacity(encoded.data.len() + 128);
                if keyframe {
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
    fn callback_context_is_retained_until_native_quiescence_is_confirmed() {
        let (tx, _rx) = mpsc::channel(1);
        let context = Box::into_raw(Box::new(FrameContext {
            output: tx,
            started: Instant::now(),
            dropped: Arc::new(AtomicU64::new(0)),
            sequence: AtomicU64::new(0),
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
