use anyhow::{Context, Result, anyhow, bail};
use apple_cf::{
    cm::CMTime,
    iosurface::{IOSurface, IOSurfaceLockOptions},
};
use bridge_core::{
    config::{Acceleration, Codec as BridgeCodec, CodecOption, H264Profile, VideoSettings},
    protocol::{MediaHeader, MediaKind},
};
use screencapturekit::{
    cm::{CMSampleBuffer, CMSampleBufferExt},
    cv::CVPixelBufferLockFlags,
    prelude::*,
};
use std::{
    ffi::c_void,
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
    fn airbridge_display_create(
        width: u32,
        height: u32,
        fps: u32,
        display_id: *mut u32,
    ) -> *mut c_void;
    fn airbridge_display_release(display: *mut c_void);
}

struct VirtualDisplay(*mut c_void);

impl VirtualDisplay {
    fn new(settings: &VideoSettings) -> Result<(Self, u32)> {
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

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
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
    },
}

struct CaptureHandler {
    output: mpsc::Sender<Captured>,
    started: Instant,
    dropped: Arc<AtomicU64>,
}

impl SCStreamOutputTrait for CaptureHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        let timestamp_us = self.started.elapsed().as_micros() as u64;
        let packet = match kind {
            SCStreamOutputType::Screen => {
                let Some(pixel) = sample.pixel_buffer() else {
                    return;
                };
                let Ok(guard) = pixel.lock(CVPixelBufferLockFlags::READ_ONLY) else {
                    return;
                };
                let Some(bytes) = (unsafe { guard.as_slice() }) else {
                    return;
                };
                Captured::Video {
                    bytes: bytes.to_vec(),
                    row_bytes: pixel.bytes_per_row(),
                    timestamp_us,
                }
            }
            _ => return,
        };
        if self.output.try_send(packet).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct EncodedPacket {
    pub header: MediaHeader,
    pub bytes: Vec<u8>,
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

pub fn capture(
    settings: VideoSettings,
    option: CodecOption,
    output: mpsc::Sender<EncodedPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<()> {
    settings.validate()?;
    if option.codec != BridgeCodec::H264 {
        bail!("The requested encoder is not available on this Mac");
    }
    let (_display, display_id) = VirtualDisplay::new(&settings)?;
    let mut display = None;
    for _ in 0..50 {
        let content = SCShareableContent::get().map_err(|error| {
            anyhow!("ScreenCaptureKit could not list displays. Grant Screen Recording permission to AirBridge, restart it, and retry: {error}")
        })?;
        display = content
            .displays()
            .into_iter()
            .find(|candidate| candidate.display_id() == display_id);
        if display.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let display = display
        .context("The virtual display did not appear in ScreenCaptureKit after five seconds")?;

    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build()?;
    let config = SCStreamConfiguration::new()
        .with_width(settings.width)
        .with_height(settings.height)
        .with_pixel_format(PixelFormat::BGRA)
        .with_minimum_frame_interval(&CMTime::new(1, settings.fps as i32))
        .with_queue_depth(3)
        .with_shows_cursor(true)
        .with_captures_audio(false)
        .with_sample_rate(48_000)
        .with_channel_count(2);
    let mut stream = SCStream::new(&filter, &config)?;
    let (captured_tx, mut captured_rx) = mpsc::channel(4);
    let started = Instant::now();
    stream.add_output_handler(
        CaptureHandler {
            output: captured_tx,
            started,
            dropped: dropped.clone(),
        },
        SCStreamOutputType::Screen,
    )?;

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
    stream.start_capture()?;

    let mut sequence = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let packet = match captured_rx.try_recv() {
            Ok(packet) => packet,
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        };
        let (header, bytes) = match packet {
            Captured::Video {
                bytes,
                row_bytes,
                timestamp_us,
            } => {
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
                    encoder.encode(&surface, CMTime::new(timestamp_us as i64, 1_000_000))?;
                if encoded.data.is_empty() {
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
                (
                    MediaHeader {
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
                )
            }
        };
        sequence += 1;
        if output
            .blocking_send(EncodedPacket { header, bytes })
            .is_err()
        {
            break;
        }
    }
    stream.stop_capture()?;
    Ok(())
}
