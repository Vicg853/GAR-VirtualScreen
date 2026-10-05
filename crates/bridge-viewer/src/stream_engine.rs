//! Concrete codec/API engines. Backend dispatch occurs at startup only.
//! Every presentation candidate owns GPU storage; decoder references stay private.
use anyhow::{Context, Result, bail};
use bridge_core::telemetry::{Outcome, Stage, TraceToken, now_us};
use gpu_video::{EncodedInputChunk, WgpuTexturesDecoder};
use h264_reader::nal::{Nal, RefNal, sps::SeqParameterSet};
use h264_reader::rbsp::BitRead;
use openh264::{
    OpenH264API,
    decoder::{Decoder, DecoderConfig, Flush},
    formats::YUVSource,
};
use std::{
    collections::{HashMap, VecDeque},
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

pub struct H264;
mod sealed {
    pub trait Api {}
}
/// No blanket implementations: only compatible codec/API pairs can instantiate.
pub trait DecodeApi<C>: sealed::Api {
    fn name(&self) -> &'static str;
    fn memory_bytes(&self) -> u64;
    fn reset(&mut self) -> Result<()>;
    fn capacity(&self) -> bool;
    fn pending_work(&self) -> bool;
    fn submit(&mut self, bytes: &[u8], pts: u64, keyframe: bool) -> Result<()>;
    fn poll(&mut self) -> Result<Vec<GpuPicture>>;
    fn submit_instrumented(
        &mut self,
        bytes: &[u8],
        pts: u64,
        keyframe: bool,
        trace: Option<&TraceToken>,
    ) -> Result<()>;
}
pub struct StreamEngine<C, A: DecodeApi<C>, const INSTRUMENTED: bool> {
    pub api: A,
    _codec: PhantomData<C>,
}
impl<C, A: DecodeApi<C>, const D: bool> StreamEngine<C, A, D> {
    pub fn new(api: A) -> Self {
        Self {
            api,
            _codec: PhantomData,
        }
    }
}

/// Texture/view handles move between owners. No decoded pixels cross a channel.
pub struct GpuPicture {
    pub storage: Arc<PictureStorage>,
    pub pts: u64,
    pub width: u32,
    pub height: u32,
    pub color: Color,
    pub planar: bool,
}
pub struct PictureStorage {
    pub views: [wgpu::TextureView; 3],
    _textures: Vec<wgpu::Texture>,
    recycle: Option<Arc<Mutex<Vec<(Vec<wgpu::Texture>, OutputPermit)>>>>,
    _permit: Option<OutputPermit>,
}
impl Drop for PictureStorage {
    fn drop(&mut self) {
        if let Some(pool) = &self.recycle {
            if let Ok(mut pool) = pool.lock() {
                if pool.len() < 4 {
                    if let Some(permit) = self._permit.take() {
                        pool.push((std::mem::take(&mut self._textures), permit));
                    }
                }
            }
        }
    }
}
const OUTPUT_LIMIT: usize = 8; // No reordering: pending <=4 plus display/in-flight leases.
struct OutputBudget(Arc<AtomicUsize>, Arc<AtomicU64>);
impl OutputBudget {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)), Arc::new(AtomicU64::new(0)))
    }
    fn acquire(&self, bytes: u64) -> Result<OutputPermit> {
        self.1
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|n| *n <= 128 * 1024 * 1024)
            })
            .map_err(|_| anyhow::anyhow!("GPU output plane budget exceeds 128 MiB"))?;
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < OUTPUT_LIMIT).then_some(count + 1)
            })
            .map_err(|_| {
                self.1.fetch_sub(bytes, Ordering::AcqRel);
                anyhow::anyhow!("GPU output lease budget exhausted")
            })?;
        Ok(OutputPermit(self.0.clone(), self.1.clone(), bytes))
    }
}
struct OutputPermit(Arc<AtomicUsize>, Arc<AtomicU64>, u64);
impl Drop for OutputPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        self.1.fetch_sub(self.2, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub matrix: u8,
    pub full_range: bool,
}
impl Default for Color {
    fn default() -> Self {
        Self {
            matrix: 1,
            full_range: false,
        }
    }
}
impl Color {
    pub fn from_sps(sps: &SeqParameterSet) -> Result<Self> {
        validate_sps(sps)?;
        let signal = sps
            .vui_parameters
            .as_ref()
            .and_then(|v| v.video_signal_type.as_ref());
        let description = signal.and_then(|s| s.colour_description.as_ref());
        if let Some(cd) = description {
            if ![1, 5, 6].contains(&cd.matrix_coefficients) {
                bail!(
                    "Unsupported SDR YUV matrix {}; HDR/BT2020 requires another engine",
                    cd.matrix_coefficients
                );
            }
            if ![1, 6, 13].contains(&cd.transfer_characteristics) {
                bail!(
                    "Unsupported transfer function {}; HDR is not silently treated as SDR",
                    cd.transfer_characteristics
                );
            }
        }
        Ok(Self {
            matrix: description.map_or(1, |cd| cd.matrix_coefficients),
            full_range: signal.is_some_and(|s| s.video_full_range_flag),
        })
    }
    pub fn coefficients(self) -> [f32; 4] {
        match self.matrix {
            5 | 6 => [1.402, -0.344136, -0.714136, 1.772],
            _ => [1.5748, -0.187324, -0.468124, 1.8556],
        }
    }
}
fn validate_sps(sps: &SeqParameterSet) -> Result<()> {
    use h264_reader::nal::sps::{ChromaFormat, FrameMbsFlags};
    if sps.chroma_info.chroma_format != ChromaFormat::YUV420
        || sps.chroma_info.bit_depth_luma_minus8 != 0
        || sps.chroma_info.bit_depth_chroma_minus8 != 0
        || sps.frame_mbs_flags != FrameMbsFlags::Frames
    {
        bail!("Only progressive 8-bit 4:2:0 H264 is supported by the negotiated engine");
    }
    if sps.max_num_ref_frames > 16 {
        bail!("H264 reference count exceeds 16");
    }
    let w = (sps.pic_width_in_mbs_minus1 as u64 + 1) * 16;
    let h = (sps.pic_height_in_map_units_minus1 as u64 + 1) * 16;
    if w > 7680 || h > 4320 || w * h > 7680 * 4320 {
        bail!("Coded picture exceeds resource limits");
    }
    if w * h * 3 / 2 * (sps.max_num_ref_frames as u64 + 2) > 256 * 1024 * 1024 {
        bail!("Decoder reference storage estimate exceeds 256 MiB per-screen budget");
    }
    if let Some(crop) = &sps.frame_cropping {
        let horizontal = (crop.left_offset as u64 + crop.right_offset as u64) * 2;
        let vertical = (crop.top_offset as u64 + crop.bottom_offset as u64) * 2;
        if horizontal >= w || vertical >= h {
            bail!("H264 crop exceeds coded picture");
        }
    }
    Ok(())
}
fn update_color(bytes: &[u8], color: &mut Color) -> Result<()> {
    // Annex-B scanner returns borrowed NAL slices, including the final NAL.
    // No duplicate compressed-picture allocation for metadata inspection.
    for nal in annex_b_nals(bytes) {
        if nal.first().is_some_and(|b| matches!(b & 31, 1 | 5)) {
            let mut bits = RefNal::new(nal, &[], true).rbsp_bits();
            bits.read_ue("first_mb_in_slice")
                .map_err(|e| anyhow::anyhow!("Invalid slice header: {e:?}"))?;
            let slice = bits
                .read_ue("slice_type")
                .map_err(|e| anyhow::anyhow!("Invalid slice type: {e:?}"))?;
            if slice > 9 || !matches!(slice % 5, 0 | 2) {
                bail!("Slice type violates negotiated progressive I/P-only video contract");
            }
        }
        if nal.first().is_some_and(|b| b & 31 == 7) {
            let parsed = SeqParameterSet::from_bits(RefNal::new(nal, &[], true).rbsp_bits())
                .map_err(|e| anyhow::anyhow!("Invalid H264 SPS: {e:?}"))?;
            *color = Color::from_sps(&parsed)?;
        }
    }
    Ok(())
}
fn annex_b_nals(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = None;
    let mut index = 0;
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        while index + 3 <= bytes.len() {
            let prefix = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if bytes[index..].starts_with(&[0, 0, 1]) {
                3
            } else {
                index += 1;
                continue;
            };
            let boundary = index;
            index += prefix;
            if let Some(previous) = start.replace(index) {
                return Some(&bytes[previous..boundary]);
            }
        }
        finished = true;
        start.take().map(|from| &bytes[from..])
    })
}

pub struct VulkanH264 {
    decoder: WgpuTexturesDecoder,
    colors: HashMap<u64, Color>,
    device: Arc<gpu_video::VulkanDevice>,
    budget: OutputBudget,
    color: Color,
}
impl sealed::Api for VulkanH264 {}
impl VulkanH264 {
    pub fn new(device: Arc<gpu_video::VulkanDevice>) -> Result<Self> {
        let mut decoder = device.create_wgpu_textures_decoder_h264(Default::default())?;
        // Protocol v6 guarantees no B/reordered pictures for this screen codec.
        decoder.set_no_reordering();
        Ok(Self {
            decoder,
            colors: HashMap::new(),
            device,
            budget: OutputBudget::new(),
            color: Color::default(),
        })
    }
}
impl DecodeApi<H264> for VulkanH264 {
    fn memory_bytes(&self) -> u64 {
        self.device.video_memory_bytes()
    }
    fn name(&self) -> &'static str {
        "Vulkan Video H264 (GPU resident)"
    }
    fn reset(&mut self) -> Result<()> {
        // Drop completes native submissions before resources can be reused.
        let mut decoder = self
            .device
            .create_wgpu_textures_decoder_h264(Default::default())?;
        decoder.set_no_reordering();
        self.decoder = decoder;
        self.colors.clear();
        self.color = Color::default();
        Ok(())
    }
    fn capacity(&self) -> bool {
        self.decoder.pending_count() < 4
    }
    fn pending_work(&self) -> bool {
        self.decoder.pending_count() > 0
    }
    fn submit(&mut self, bytes: &[u8], pts: u64, keyframe: bool) -> Result<()> {
        let _ = keyframe;
        update_color(bytes, &mut self.color)?;
        if self.colors.len() >= 32 {
            bail!("Vulkan picture metadata budget exhausted");
        }
        self.decoder.submit_picture(EncodedInputChunk {
            data: bytes,
            pts: Some(pts),
        })?;
        self.colors.insert(pts, self.color);
        Ok(())
    }
    fn submit_instrumented(
        &mut self,
        bytes: &[u8],
        pts: u64,
        keyframe: bool,
        trace: Option<&TraceToken>,
    ) -> Result<()> {
        let _ = keyframe;
        update_color(bytes, &mut self.color)?;
        if self.colors.len() >= 32 {
            bail!("Vulkan picture metadata budget exhausted");
        }
        let start = now_us();
        let timing = self.decoder.submit_picture_timed(EncodedInputChunk {
            data: bytes,
            pts: Some(pts),
        })?;
        self.colors.insert(pts, self.color);
        if let Some(t) = trace {
            t.mark(
                Stage::CpuParse,
                start,
                Some(start + timing.parse_us),
                Outcome::Complete,
                "CPU Annex-B parsing / codec dependency metadata",
            );
            if let Some((offset, duration)) = timing.upload {
                t.mark(
                    Stage::GpuInput,
                    start + offset,
                    Some(start + offset + duration),
                    Outcome::Complete,
                    "Mapped coherent bitstream write; not GPU decode execution",
                );
            } else {
                t.mark(
                    Stage::GpuInput,
                    start,
                    None,
                    Outcome::Unavailable,
                    "No picture upload in this access unit",
                );
            }
            t.mark(
                Stage::DecodeSubmit,
                start + timing.parse_us,
                Some(start + timing.parse_us + timing.submission_us),
                Outcome::Complete,
                "Session/command preparation, bitstream upload and decode/copy submission",
            );
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<Vec<GpuPicture>> {
        self.device.wgpu_device().poll(wgpu::PollType::Poll)?;
        let mut output = Vec::new();
        for frame in self.decoder.poll()? {
            let pts = frame
                .metadata
                .pts
                .context("Vulkan decoder omitted picture timestamp")?;
            let color = self
                .colors
                .remove(&pts)
                .context("Vulkan decoder omitted per-picture color correlation")?;
            let texture = frame.data;
            let width = texture.width();
            let height = texture.height();
            let y = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::R8Unorm),
                aspect: wgpu::TextureAspect::Plane0,
                ..Default::default()
            });
            let uv = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rg8Unorm),
                aspect: wgpu::TextureAspect::Plane1,
                ..Default::default()
            });
            output.push(GpuPicture {
                storage: Arc::new(PictureStorage {
                    views: [y, uv.clone(), uv],
                    _textures: vec![texture],
                    recycle: None,
                    _permit: Some(self.budget.acquire(width as u64 * height as u64 * 3 / 2)?),
                }),
                pts,
                width,
                height,
                color,
                planar: false,
            });
        }
        Ok(output)
    }
}

pub struct SoftwareH264 {
    decoder: Decoder,
    device: wgpu::Device,
    queue: wgpu::Queue,
    budget: OutputBudget,
    pending_pts: VecDeque<(u64, Color)>,
    ready: Vec<GpuPicture>,
    uploads: Arc<AtomicUsize>,
    texture_pool: Arc<Mutex<Vec<(Vec<wgpu::Texture>, OutputPermit)>>>,
    color: Color,
}
impl sealed::Api for SoftwareH264 {}
impl SoftwareH264 {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self> {
        Ok(Self {
            decoder: software_decoder()?,
            device,
            queue,
            budget: OutputBudget::new(),
            pending_pts: VecDeque::new(),
            ready: Vec::new(),
            uploads: Arc::new(AtomicUsize::new(0)),
            texture_pool: Arc::new(Mutex::new(Vec::new())),
            color: Color::default(),
        })
    }
}
fn software_decoder() -> Result<Decoder> {
    Decoder::with_api_config(
        OpenH264API::from_source(),
        DecoderConfig::new().flush_after_decode(Flush::NoFlush),
    )
    .context("Direct OpenH264 initialization")
}
impl DecodeApi<H264> for SoftwareH264 {
    fn memory_bytes(&self) -> u64 {
        self.budget.1.load(Ordering::Acquire)
    }
    fn name(&self) -> &'static str {
        "OpenH264 software / Vulkan (one plane upload)"
    }
    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.texture_pool.lock().unwrap().clear();
        self.pending_pts.clear();
        self.color = Color::default();
        self.decoder = software_decoder()?;
        Ok(())
    }
    fn capacity(&self) -> bool {
        self.uploads.load(Ordering::Acquire) < 2
    }
    fn pending_work(&self) -> bool {
        self.uploads.load(Ordering::Acquire) > 0
    }
    fn submit(&mut self, bytes: &[u8], pts: u64, keyframe: bool) -> Result<()> {
        self.submit_typed::<false>(bytes, pts, keyframe, None)
    }
    fn submit_instrumented(
        &mut self,
        bytes: &[u8],
        pts: u64,
        keyframe: bool,
        trace: Option<&TraceToken>,
    ) -> Result<()> {
        self.submit_typed::<true>(bytes, pts, keyframe, trace)
    }
    fn poll(&mut self) -> Result<Vec<GpuPicture>> {
        self.device.poll(wgpu::PollType::Poll)?;
        Ok(std::mem::take(&mut self.ready))
    }
}

impl SoftwareH264 {
    fn submit_typed<const DEBUG: bool>(
        &mut self,
        bytes: &[u8],
        pts: u64,
        keyframe: bool,
        trace: Option<&TraceToken>,
    ) -> Result<()> {
        let parse_start = if DEBUG { now_us() } else { 0 };
        let _ = keyframe;
        update_color(bytes, &mut self.color)?;
        if self.pending_pts.len() >= 32 {
            bail!("Software decoder exceeded bounded picture correlation");
        }
        self.pending_pts.push_back((pts, self.color));
        let decode_start = if DEBUG { now_us() } else { 0 };
        if DEBUG {
            if let Some(t) = trace {
                t.mark(
                    Stage::CpuParse,
                    parse_start,
                    Some(decode_start),
                    Outcome::Complete,
                    "SPS metadata validation",
                );
            }
        }
        let Some(yuv) = self
            .decoder
            .decode(bytes)
            .context("Direct OpenH264 decode")?
        else {
            return Ok(());
        };
        let upload_start = if DEBUG { now_us() } else { 0 };
        if DEBUG {
            if let Some(t) = trace {
                t.mark(
                    Stage::SoftwareDecode,
                    decode_start,
                    Some(upload_start),
                    Outcome::Complete,
                    "Direct OpenH264 CPU decode",
                );
            }
        }
        let (pts, color) = self
            .pending_pts
            .pop_front()
            .context("Software decoder emitted an unstamped picture")?;
        let (width, height) = yuv.dimensions();
        if width == 0
            || height == 0
            || width > 7680
            || height > 4320
            || width % 2 != 0
            || height % 2 != 0
        {
            bail!("Unsupported decoded I420 dimensions {width}x{height}");
        }
        let cached = self.texture_pool.lock().unwrap().pop();
        let (mut cached, permit) = match cached.filter(|(textures, _)| {
            textures.len() == 3
                && textures[0].width() == width as u32
                && textures[0].height() == height as u32
        }) {
            Some(cached) => cached,
            None => (
                Vec::new(),
                self.budget.acquire(width as u64 * height as u64 * 3 / 2)?,
            ),
        };
        let strides = yuv.strides();
        let mut textures = Vec::with_capacity(3);
        for (data, stride, w, h) in [
            (yuv.y(), strides.0, width, height),
            (yuv.u(), strides.1, width / 2, height / 2),
            (yuv.v(), strides.2, width / 2, height / 2),
        ] {
            let old = if cached.is_empty() {
                None
            } else {
                Some(cached.remove(0))
            };
            let texture = match old
                .filter(|texture| texture.width() == w as u32 && texture.height() == h as u32)
            {
                Some(texture) => texture,
                None => self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("AirBridge software decoded plane"),
                    size: wgpu::Extent3d {
                        width: w as u32,
                        height: h as u32,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                }),
            };
            // Queue copies borrowed decoder planes directly into upload storage.
            // No intermediate I420 picture or CPU decoded-frame queue.
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h as u32),
                },
                wgpu::Extent3d {
                    width: w as u32,
                    height: h as u32,
                    depth_or_array_layers: 1,
                },
            );
            textures.push(texture);
        }
        // Flush upload even when the window is occluded; staging allocations must
        // not accumulate waiting for the next visible rendering submission.
        self.uploads.fetch_add(1, Ordering::AcqRel);
        self.queue.submit([]);
        let uploads = self.uploads.clone();
        self.queue.on_submitted_work_done(move || {
            uploads.fetch_sub(1, Ordering::AcqRel);
        });
        let views = std::array::from_fn(|i| textures[i].create_view(&Default::default()));
        self.ready.push(GpuPicture {
            storage: Arc::new(PictureStorage {
                views,
                _textures: textures,
                recycle: Some(self.texture_pool.clone()),
                _permit: Some(permit),
            }),
            pts,
            width: width as u32,
            height: height as u32,
            color,
            planar: true,
        });
        if DEBUG {
            if let Some(t) = trace {
                t.mark(
                    Stage::SoftwareUpload,
                    upload_start,
                    Some(now_us()),
                    Outcome::Complete,
                    "One decoded-plane upload and submission; completion remains asynchronous",
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn annex_b_metadata_scanner_keeps_final_nal() {
        let bytes = [0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x65, 8];
        assert_eq!(
            annex_b_nals(&bytes).collect::<Vec<_>>(),
            vec![&[0x67, 9][..], &[0x65, 8][..]]
        );
    }
    #[test]
    fn output_budget_is_bounded_and_reusable() {
        let budget = OutputBudget::new();
        let mut held = (0..OUTPUT_LIMIT)
            .map(|_| budget.acquire(1024).unwrap())
            .collect::<Vec<_>>();
        assert!(budget.acquire(1024).is_err());
        held.pop();
        assert!(budget.acquire(1024).is_ok());
    }
    #[test]
    fn reordered_slices_violate_wire_contract() {
        // ue(first_mb=0), ue(slice_type=B=1), rbsp stop bit.
        assert!(update_color(&[0, 0, 1, 0x41, 0b10101000], &mut Color::default()).is_err());
    }
    #[test]
    fn matrices_have_distinct_coefficients() {
        assert_ne!(
            Color::default().coefficients(),
            Color {
                matrix: 6,
                full_range: false
            }
            .coefficients()
        );
    }
}

#[cfg(test)]
mod codec_tests {
    use super::*;
    use openh264::{encoder::Encoder, formats::YUVBuffer};
    fn noop_device() -> (wgpu::Device, wgpu::Queue) {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::NOOP;
        descriptor.backend_options.noop.enable = true;
        let instance = wgpu::Instance::new(descriptor);
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
        pollster::block_on(adapter.request_device(&Default::default())).unwrap()
    }
    fn fixture() -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new().unwrap();
        let yuv = YUVBuffer::new(64, 64);
        (0..30)
            .filter_map(|_| {
                let bytes = encoder.encode(&yuv).unwrap().to_vec();
                (!bytes.is_empty()).then_some(bytes)
            })
            .collect()
    }
    #[test]
    fn direct_decode_upload_has_identical_pts_in_both_compiled_modes() {
        let (device, queue) = noop_device();
        let mut normal = SoftwareH264::new(device.clone(), queue.clone()).unwrap();
        let mut measured = SoftwareH264::new(device, queue).unwrap();
        let pictures = fixture();
        let mut normal_pts = Vec::new();
        let mut measured_pts = Vec::new();
        for (index, bytes) in pictures.iter().enumerate() {
            let pts = 2_000_000 + index as u64 * 16_667;
            normal.submit(bytes, pts, index == 0).unwrap();
            measured
                .submit_instrumented(bytes, pts, index == 0, None)
                .unwrap();
            normal_pts.extend(
                normal
                    .poll()
                    .unwrap()
                    .into_iter()
                    .map(|picture| picture.pts),
            );
            measured_pts.extend(
                measured
                    .poll()
                    .unwrap()
                    .into_iter()
                    .map(|picture| picture.pts),
            );
        }
        assert!(!normal_pts.is_empty());
        assert_eq!(normal_pts, measured_pts);
        assert!(normal_pts.windows(2).all(|pair| pair[0] < pair[1]));
    }
    #[test]
    fn repeated_decoder_reset_and_output_recycling_stay_bounded() {
        let (device, queue) = noop_device();
        let mut engine = SoftwareH264::new(device, queue).unwrap();
        let fixture = fixture();
        for generation in 0..100 {
            engine.reset().unwrap();
            for (index, bytes) in fixture.iter().enumerate() {
                engine
                    .submit(
                        bytes,
                        generation * 1_000_000 + index as u64 * 16_667,
                        index == 0,
                    )
                    .unwrap();
                drop(engine.poll().unwrap());
                assert!(engine.memory_bytes() <= 8 * 64 * 64 * 3 / 2);
                assert!(engine.texture_pool.lock().unwrap().len() <= 4);
            }
        }
        engine.reset().unwrap();
        assert_eq!(engine.memory_bytes(), 0);
    }
    #[test]
    fn corrupt_sps_is_a_result_error() {
        let mut color = Color::default();
        assert!(update_color(&[0, 0, 0, 1, 0x67, 0xff], &mut color).is_err());
    }
}
