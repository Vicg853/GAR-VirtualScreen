use anyhow::{Context, Result, anyhow, bail};
use bridge_core::diagnostics::Diagnostics;
use bytemuck::{Pod, Zeroable};
use hang::catalog::{H264, VideoConfig};
use moq_net::Timestamp;
use moq_video::{Frame as DecodedVideoFrame, decode, render};
use render::wgpu;
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
};

const MAX_QUEUED_PACKET_BYTES: usize = bridge_core::protocol::MAX_MEDIA;
const MAX_FRAME_PIXELS: u64 = 7680 * 4320;

#[derive(Default)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub skipped: u64,
    pub decoder: String,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct VideoWindow {
    input: async_mpsc::Sender<VideoInput>,
    proxy: Option<EventLoopProxy<VideoEvent>>,
    unavailable: Option<String>,
}

impl VideoWindow {
    pub fn spawn(target: Arc<Mutex<VideoFrame>>, diagnostics: Arc<Diagnostics>) -> Result<Self> {
        let (input, input_rx) = async_mpsc::channel(1);
        let (proxy_tx, proxy_rx) =
            mpsc::sync_channel::<std::result::Result<EventLoopProxy<VideoEvent>, String>>(1);
        let thread_target = target.clone();
        let thread_diagnostics = diagnostics.clone();
        thread::Builder::new()
            .name("airbridge-video-window".into())
            .spawn(move || {
                let mut builder = EventLoop::<VideoEvent>::with_user_event();
                winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(
                    &mut builder,
                    true,
                );
                winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
                let event_loop = match builder.build() {
                    Ok(event_loop) => event_loop,
                    Err(error) => {
                        let message = format!("Cannot initialize Linux video window: {error}");
                        thread_diagnostics.record("video-window", &message);
                        record_error(&thread_target, message.clone());
                        let _ = proxy_tx.send(Err(message));
                        return;
                    }
                };
                let proxy = event_loop.create_proxy();
                if proxy_tx.send(Ok(proxy.clone())).is_err() {
                    return;
                }
                let mut app = VideoApp::new(
                    input_rx,
                    thread_target.clone(),
                    thread_diagnostics.clone(),
                    proxy,
                );
                if let Err(error) = event_loop.run_app(&mut app) {
                    let message = format!("Video window event loop stopped: {error}");
                    thread_diagnostics.record("video-window", &message);
                    record_error(&thread_target, message);
                }
            })
            .context("Cannot start the native video-window thread")?;

        let proxy = proxy_rx
            .recv_timeout(Duration::from_secs(10))
            .context("Timed out initializing the native video window")?
            .map_err(|error| anyhow!(error))?;
        Ok(Self {
            input,
            proxy: Some(proxy),
            unavailable: None,
        })
    }

    pub fn unavailable(message: String, target: Arc<Mutex<VideoFrame>>) -> Self {
        let (input, receiver) = async_mpsc::channel(1);
        drop(receiver);
        record_error(&target, message.clone());
        Self {
            input,
            proxy: None,
            unavailable: Some(message),
        }
    }

    pub fn show(&self) -> Result<()> {
        self.send_event(VideoEvent::Show)
    }

    pub async fn reset(&self, width: u32, height: u32) -> Result<()> {
        self.send_input(VideoInput::Reset { width, height }).await
    }

    pub async fn submit(&self, bytes: Vec<u8>, keyframe: bool) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_QUEUED_PACKET_BYTES {
            bail!("Video packet size is outside the supported range");
        }
        self.send_input(VideoInput::Packet { bytes, keyframe })
            .await
    }

    async fn send_input(&self, input: VideoInput) -> Result<()> {
        if let Some(message) = &self.unavailable {
            bail!("{message}");
        }
        tokio::time::timeout(Duration::from_secs(5), self.input.send(input))
            .await
            .context("The native video window has not accepted a frame for five seconds")?
            .context("The native video window has stopped")?;
        self.send_event(VideoEvent::Wake)
    }

    fn send_event(&self, event: VideoEvent) -> Result<()> {
        if let Some(message) = &self.unavailable {
            bail!("{message}");
        }
        self.proxy
            .as_ref()
            .context("The native video window is unavailable")?
            .send_event(event)
            .map_err(|_| anyhow!("The native video window has stopped"))
    }
}

enum VideoEvent {
    Wake,
    Show,
}

#[derive(Debug)]
enum VideoInput {
    Reset { width: u32, height: u32 },
    Packet { bytes: Vec<u8>, keyframe: bool },
}

enum DecodedFrame {
    Reset,
    Video(DecodedVideoFrame),
}

struct DecodeBackend {
    decoder: decode::Decoder,
    catalog: VideoConfig,
    config: decode::Config,
}

impl DecodeBackend {
    fn reset(&mut self) -> Result<()> {
        self.decoder = decode::Decoder::new(&self.catalog, &self.config)
            .context("Cannot reset the video decoder")?;
        Ok(())
    }

    fn name(&self) -> &str {
        self.decoder.name()
    }

    fn is_hardware(&self) -> bool {
        matches!(
            &self.config.kind,
            decode::Kind::Hardware | decode::Kind::Named(_)
        )
    }

    fn fallback(&mut self) -> Result<Vec<String>> {
        const HARDWARE_BACKENDS: [&str; 3] = ["nvdec", "vaapi", "v4l2"];
        let current = self.name();
        let next = HARDWARE_BACKENDS
            .iter()
            .position(|name| *name == current)
            .map_or(HARDWARE_BACKENDS.len(), |index| index + 1);
        let mut refused = Vec::new();

        for name in HARDWARE_BACKENDS.iter().skip(next) {
            self.config.kind = decode::Kind::Named((*name).into());
            match decode::Decoder::new(&self.catalog, &self.config) {
                Ok(decoder) => {
                    self.decoder = decoder;
                    return Ok(refused);
                }
                Err(error) => refused.push(format!("{name}: {error:#}")),
            }
        }

        self.config.kind = decode::Kind::Software;
        self.decoder = decode::Decoder::new(&self.catalog, &self.config)
            .context("Cannot initialize the OpenH264 software fallback")?;
        Ok(refused)
    }
}

struct VideoApp {
    input_rx: async_mpsc::Receiver<VideoInput>,
    pending_input: Option<VideoInput>,
    target: Arc<Mutex<VideoFrame>>,
    diagnostics: Arc<Diagnostics>,
    proxy: EventLoopProxy<VideoEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    latest_frame: Option<PresentedFrame>,
    decoder_tx: Option<mpsc::SyncSender<VideoInput>>,
    decoded_rx: Option<mpsc::Receiver<DecodedFrame>>,
    initialized: bool,
}

impl VideoApp {
    fn new(
        input_rx: async_mpsc::Receiver<VideoInput>,
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
        proxy: EventLoopProxy<VideoEvent>,
    ) -> Self {
        Self {
            input_rx,
            pending_input: None,
            target,
            diagnostics,
            proxy,
            window: None,
            renderer: None,
            latest_frame: None,
            decoder_tx: None,
            decoded_rx: None,
            initialized: false,
        }
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        let attributes = Window::default_attributes()
            .with_title("AirBridge Display")
            .with_inner_size(PhysicalSize::new(1280, 720));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.report_error(format!("Cannot create the display window: {error}"));
                return;
            }
        };
        self.window = Some(window.clone());

        let (renderer, adapter_name, dma_buf_import) = match initialize_wgpu_renderer(&window) {
            Ok(renderer) => renderer,
            Err(error) => {
                self.report_error(format!(
                    "GPU display initialization failed; decoded frames cannot be presented: {error:#}"
                ));
                return;
            }
        };
        if let Ok(mut status) = self.target.lock() {
            status.decoder = "Probing hardware decoders…".into();
        }
        self.diagnostics.record(
            "renderer",
            format!(
                "Presenting on {adapter_name}; Vulkan DMA-BUF import {}",
                if dma_buf_import {
                    "capability available; incompatible surfaces fall back to upload"
                } else {
                    "unavailable; upload fallback active"
                }
            ),
        );
        self.renderer = Some(renderer);

        let (decoder_tx, decoder_rx) = mpsc::sync_channel(1);
        let (decoded_tx, decoded_rx) = mpsc::sync_channel(1);
        self.decoder_tx = Some(decoder_tx);
        self.decoded_rx = Some(decoded_rx);
        let target = self.target.clone();
        let diagnostics = self.diagnostics.clone();
        let proxy = self.proxy.clone();
        if let Err(error) = thread::Builder::new()
            .name("airbridge-video-decoder".into())
            .spawn(move || decode_loop(decoder_rx, decoded_tx, target, diagnostics, proxy))
        {
            self.report_error(format!("Cannot start the video decoder worker: {error}"));
            self.decoder_tx = None;
        }
    }

    fn report_error(&self, message: String) {
        self.diagnostics.record("video-window", &message);
        record_error(&self.target, message);
    }

    fn pump_input(&mut self) {
        if self.pending_input.is_none() {
            self.pending_input = self.input_rx.try_recv().ok();
        }
        let Some(input) = self.pending_input.take() else {
            return;
        };
        if let VideoInput::Reset { width, height } = &input {
            self.latest_frame = None;
            if let Ok(mut status) = self.target.lock() {
                status.width = *width;
                status.height = *height;
                status.frames = 0;
                status.skipped = 0;
                status.error = None;
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        let Some(sender) = &self.decoder_tx else {
            self.pending_input = Some(input);
            return;
        };
        match sender.try_send(input) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(input)) => self.pending_input = Some(input),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.report_error("The video decoder worker has stopped".into());
                self.decoder_tx = None;
            }
        }
    }

    fn collect_decoded(&mut self) {
        let Some(receiver) = &self.decoded_rx else {
            return;
        };
        let mut latest = None;
        while let Ok(decoded) = receiver.try_recv() {
            latest = Some(decoded);
        }
        if let Some(decoded) = latest {
            match decoded {
                DecodedFrame::Reset => {
                    self.latest_frame = None;
                }
                decoded => {
                    if let Some(renderer) = &mut self.renderer {
                        match renderer.set_frame(decoded) {
                            Ok(frame) => self.latest_frame = Some(frame),
                            Err(error) => self.report_error(format!(
                                "Could not prepare a decoded frame for display: {error:#}"
                            )),
                        }
                    }
                }
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }
}

impl ApplicationHandler<VideoEvent> for VideoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.initialize(event_loop);
        if let Some(window) = &self.window {
            window.set_visible(true);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: VideoEvent) {
        match event {
            VideoEvent::Wake => {}
            VideoEvent::Show => {
                if let Some(window) = &self.window {
                    window.set_visible(true);
                    window.focus_window();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self
            .window
            .as_ref()
            .filter(|window| window.id() == window_id)
        else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => window.set_visible(false),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(window.inner_size());
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer {
                    if let Err(error) = renderer.render(self.latest_frame.as_ref()) {
                        self.report_error(format!("Video presentation failed: {error:#}"));
                    }
                }
            }
            WindowEvent::Destroyed => event_loop.exit(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.pump_input();
        self.collect_decoded();
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

fn initialize_wgpu_renderer(window: &Arc<Window>) -> Result<(Renderer, String, bool)> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let surface = instance
        .create_surface(window.clone())
        .context("Cannot create the GPU display surface")?;
    let adapter_options = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    };
    let adapter = match pollster::block_on(instance.request_adapter(&adapter_options)) {
        Ok(adapter) => adapter,
        Err(_) => pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..adapter_options
        }))
        .context("No GPU or software GPU is available for video presentation")?,
    };
    let info = adapter.get_info();
    let supported = adapter.features();
    let dma_buf_import = supported.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF);
    let required_features = if dma_buf_import {
        wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
    } else {
        wgpu::Features::empty()
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("AirBridge video renderer"),
        required_features,
        ..Default::default()
    }))
    .context("Cannot create the GPU presentation device")?;
    let renderer = Renderer::new(surface, adapter, device, queue, window.inner_size())?;
    Ok((
        renderer,
        format!("{} ({:?})", info.name, info.backend),
        dma_buf_import,
    ))
}

fn decode_loop(
    receiver: mpsc::Receiver<VideoInput>,
    output: mpsc::SyncSender<DecodedFrame>,
    target: Arc<Mutex<VideoFrame>>,
    diagnostics: Arc<Diagnostics>,
    proxy: EventLoopProxy<VideoEvent>,
) {
    let catalog = VideoConfig::new(H264 {
        inline: true,
        profile: 0x64,
        constraints: 0,
        level: 0x28,
    });
    let mut decoder_config = decode::Config::new();
    decoder_config.kind = decode::Kind::Hardware;
    decoder_config.output = moq_video::Output::Native;
    let decoder = match decode::Decoder::new(&catalog, &decoder_config) {
        Ok(decoder) => decoder,
        Err(hardware_error) => {
            diagnostics.record(
                "decoder",
                format!(
                    "No H.264 hardware decoder is available: {hardware_error:#}; trying OpenH264"
                ),
            );
            decoder_config.kind = decode::Kind::Software;
            match decode::Decoder::new(&catalog, &decoder_config) {
                Ok(decoder) => decoder,
                Err(software_error) => {
                    let message = format!(
                        "Could not initialize video decoding; hardware: {hardware_error:#}; software: {software_error:#}"
                    );
                    diagnostics.record("decoder", &message);
                    record_error(&target, message);
                    return;
                }
            }
        }
    };
    let mut backend = DecodeBackend {
        decoder,
        catalog,
        config: decoder_config,
    };
    diagnostics.record(
        "decoder",
        format!(
            "Using {} H.264 {} decoding",
            backend.name(),
            if backend.is_hardware() {
                "hardware"
            } else {
                "software"
            }
        ),
    );
    update_decoder_status(&target, backend.name());
    let mut packet_count = 0u64;
    let mut error_count = 0u64;
    while let Ok(input) = receiver.recv() {
        match input {
            VideoInput::Reset { width, height } => {
                packet_count = 0;
                error_count = 0;
                if let Ok(mut status) = target.lock() {
                    status.width = width;
                    status.height = height;
                    status.frames = 0;
                    status.skipped = 0;
                    status.error = None;
                }
                if let Err(error) = backend.reset() {
                    let message = format!("Could not reset the video decoder: {error:#}");
                    diagnostics.record("decoder", &message);
                    record_error(&target, message);
                    if backend.is_hardware() {
                        match backend.fallback() {
                            Ok(refused) => {
                                diagnostics.record(
                                    "decoder",
                                    format!(
                                        "Reset fallback selected {}; later hardware backends refused: {}",
                                        backend.name(),
                                        refused.join("; ")
                                    ),
                                );
                                update_decoder_status(&target, backend.name());
                            }
                            Err(fallback_error) => {
                                let message =
                                    format!("Software decoder reset failed: {fallback_error:#}");
                                diagnostics.record("decoder", &message);
                                record_error(&target, message);
                                return;
                            }
                        }
                    } else {
                        return;
                    }
                }
                if output.send(DecodedFrame::Reset).is_err() {
                    return;
                }
            }
            VideoInput::Packet { bytes, keyframe } => {
                packet_count += 1;
                let timestamp = Timestamp::from_micros(packet_count).unwrap_or(Timestamp::ZERO);
                let payload = bytes::Bytes::from(bytes);
                let frames = backend
                    .decoder
                    .decode(&payload, timestamp, keyframe)
                    .map(|frames| frames.into_iter().map(DecodedFrame::Video).collect())
                    .map_err(|error| anyhow!(error));
                match frames {
                    Ok(frames) => {
                        for frame in frames {
                            if !publish_frame(frame, &output, &target) {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        error_count += 1;
                        if backend.is_hardware() {
                            let failed_backend = backend.name().to_owned();
                            let message = format!(
                                "{failed_backend} failed on packet {packet_count}: {error:#}; trying the next available decoder"
                            );
                            diagnostics.record("decoder", &message);
                            record_error(
                                &target,
                                format!("{message}; waiting for the next keyframe"),
                            );
                            match backend.fallback() {
                                Ok(refused) => {
                                    let message = format!(
                                        "Decoder fallback selected {}; later hardware backends refused: {}",
                                        backend.name(),
                                        refused.join("; ")
                                    );
                                    diagnostics.record("decoder", &message);
                                    update_decoder_status(&target, backend.name());
                                }
                                Err(fallback_error) => {
                                    let message = format!(
                                        "Cannot start remaining hardware or OpenH264 software decoders: {fallback_error:#}"
                                    );
                                    diagnostics.record("decoder", &message);
                                    record_error(&target, message);
                                    return;
                                }
                            }
                            if keyframe {
                                loop {
                                    match backend.decoder.decode(&payload, timestamp, keyframe) {
                                        Ok(frames) => {
                                            for frame in frames {
                                                if !publish_frame(
                                                    DecodedFrame::Video(frame),
                                                    &output,
                                                    &target,
                                                ) {
                                                    return;
                                                }
                                            }
                                            break;
                                        }
                                        Err(error) if backend.is_hardware() => {
                                            let failed_backend = backend.name().to_owned();
                                            diagnostics.record(
                                                "decoder",
                                                format!(
                                                    "{failed_backend} rejected the keyframe; trying the next decoder: {error:#}"
                                                ),
                                            );
                                            match backend.fallback() {
                                                Ok(refused) => {
                                                    diagnostics.record(
                                                        "decoder",
                                                        format!(
                                                            "Decoder fallback selected {}; later hardware backends refused: {}",
                                                            backend.name(),
                                                            refused.join("; ")
                                                        ),
                                                    );
                                                    update_decoder_status(&target, backend.name());
                                                }
                                                Err(fallback_error) => {
                                                    let message = format!(
                                                        "Cannot start remaining hardware or OpenH264 software decoders: {fallback_error:#}"
                                                    );
                                                    diagnostics.record("decoder", &message);
                                                    record_error(&target, message);
                                                    return;
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            diagnostics.record(
                                                "decoder",
                                                format!(
                                                    "{} could not decode fallback keyframe: {error:#}",
                                                    backend.name()
                                                ),
                                            );
                                            break;
                                        }
                                    }
                                }
                            }
                        } else {
                            let message = format!("H.264 decoder error: {error:#}");
                            if error_count == 1 || error_count % 60 == 0 {
                                diagnostics.record("decoder", &message);
                                record_error(&target, format!("{message} · packet {packet_count}"));
                            }
                        }
                    }
                }
            }
        }
        let _ = proxy.send_event(VideoEvent::Wake);
    }
}

fn publish_frame(
    frame: DecodedFrame,
    output: &mpsc::SyncSender<DecodedFrame>,
    target: &Arc<Mutex<VideoFrame>>,
) -> bool {
    let (width, height) = match &frame {
        DecodedFrame::Video(frame) => (frame.size().width, frame.size().height),
        DecodedFrame::Reset => return true,
    };
    if width == 0
        || height == 0
        || width > 7680
        || height > 4320
        || (width as u64) * (height as u64) > MAX_FRAME_PIXELS
    {
        record_error(
            target,
            format!("Decoded frame dimensions are unsupported: {width}×{height}"),
        );
        return true;
    }
    if let Ok(mut status) = target.lock() {
        status.width = width;
        status.height = height;
        status.frames += 1;
        status.error = None;
    }
    match output.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_)) => {
            if let Ok(mut status) = target.lock() {
                status.skipped += 1;
            }
            true
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

fn update_decoder_status(target: &Arc<Mutex<VideoFrame>>, name: &str) {
    if let Ok(mut status) = target.lock() {
        status.decoder = name.into();
    }
}

fn record_error(target: &Arc<Mutex<VideoFrame>>, message: String) {
    if let Ok(mut status) = target.lock() {
        status.error = Some(message);
    }
}

struct PresentedFrame {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorUniform {
    values: [f32; 4],
    coefficients: [f32; 4],
    output: [f32; 4],
}

struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    configuration: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,
    dummy_uv: wgpu::Texture,
    frame_renderer: render::Renderer,
    size: PhysicalSize<u32>,
}

impl Renderer {
    fn new(
        surface: wgpu::Surface<'static>,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        size: PhysicalSize<u32>,
    ) -> Result<Self> {
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .or_else(|| capabilities.formats.first().copied())
            .context("Display surface exposes no supported pixel formats")?;
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &configuration);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("AirBridge decoded-frame layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("AirBridge color-conversion parameters"),
            contents: bytemuck::bytes_of(&ColorUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("AirBridge YUV and RGBA presentation shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("AirBridge video pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("AirBridge native video renderer"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let dummy_uv = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("AirBridge packed-frame dummy chroma plane"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let frame_renderer = render::Renderer::new(&device, &queue, render::Config::new())
            .context("Cannot initialize the cross-vendor GPU frame importer")?;

        Ok(Self {
            surface,
            device,
            queue,
            configuration,
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buffer,
            dummy_uv,
            frame_renderer,
            size,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        self.size = size;
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.configuration.width = size.width;
        self.configuration.height = size.height;
        self.surface.configure(&self.device, &self.configuration);
    }

    fn set_frame(&mut self, decoded: DecodedFrame) -> Result<PresentedFrame> {
        let (texture, y_view, uv_view, width, height, color) = match decoded {
            DecodedFrame::Video(frame) => {
                let size = frame.size();
                if size.width == 0
                    || size.height == 0
                    || size.width > 7680
                    || size.height > 4320
                    || size.pixels() > MAX_FRAME_PIXELS
                {
                    bail!("Decoded frame exceeds the 7680×4320 presentation limit");
                }
                let rendered = self
                    .frame_renderer
                    .render(&frame)
                    .context("Cross-vendor GPU frame import/render failed")?;
                let y_view = rendered.create_view(&wgpu::TextureViewDescriptor::default());
                let uv_view = self
                    .dummy_uv
                    .create_view(&wgpu::TextureViewDescriptor::default());
                (
                    rendered,
                    y_view,
                    uv_view,
                    size.width,
                    size.height,
                    ColorUniform {
                        // The cross-vendor frame renderer already converted the
                        // source's YUV matrix/range into gamma-coded RGBA.
                        values: [0.0, 1.0, 1.0, 1.0],
                        coefficients: [1.5748, -0.187324, -0.468124, 1.8556],
                        output: [
                            if self.configuration.format.is_srgb() {
                                1.0
                            } else {
                                0.0
                            },
                            0.0,
                            0.0,
                            0.0,
                        ],
                    },
                )
            }
            DecodedFrame::Reset => bail!("Reset is not a displayable video frame"),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&color));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("AirBridge current video frame"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });
        Ok(PresentedFrame {
            _texture: texture,
            bind_group,
            width,
            height,
        })
    }

    fn render(&mut self, frame: Option<&PresentedFrame>) -> Result<()> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(());
        }
        let output = match self.surface.get_current_texture() {
            Ok(output) => output,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(error) => return Err(anyhow!(error)),
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("AirBridge display frame"),
            });
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("AirBridge display render pass"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(frame) = frame {
                let surface_width = self.size.width as f32;
                let surface_height = self.size.height as f32;
                let image_aspect = frame.width as f32 / frame.height as f32;
                let surface_aspect = surface_width / surface_height;
                let (x, y, width, height) = if surface_aspect > image_aspect {
                    let width = surface_height * image_aspect;
                    ((surface_width - width) / 2.0, 0.0, width, surface_height)
                } else {
                    let height = surface_width / image_aspect;
                    (0.0, (surface_height - height) / 2.0, surface_width, height)
                };
                pass.set_viewport(x, y, width, height, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &frame.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

const VIDEO_SHADER: &str = r#"
struct ColorUniform {
    values: vec4<f32>,
    coefficients: vec4<f32>,
    output: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var plane_y_or_rgba: texture_2d<f32>;
@group(0) @binding(1) var plane_uv: texture_2d<f32>;
@group(0) @binding(2) var frame_sampler: sampler;
@group(0) @binding(3) var<uniform> color: ColorUniform;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    let position = positions[index];
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.uv = vec2<f32>(position.x * 0.5 + 0.5, 0.5 - position.y * 0.5);
    return output;
}

fn srgb_to_linear(value: vec3<f32>) -> vec3<f32> {
    let low = value / 12.92;
    let high = pow((value + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(low, high, value > vec3<f32>(0.04045));
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    var rgb: vec3<f32>;
    if color.values.w > 0.5 {
        rgb = textureSample(plane_y_or_rgba, frame_sampler, input.uv).rgb;
    } else {
        let luma = textureSample(plane_y_or_rgba, frame_sampler, input.uv).r;
        let chroma = textureSample(plane_uv, frame_sampler, input.uv).rg - vec2<f32>(0.5, 0.5);
        let y = (luma + color.values.x) * color.values.y;
        let u = chroma.x * color.values.z;
        let v = chroma.y * color.values.z;
        rgb = vec3<f32>(
            y + color.coefficients.x * v,
            y + color.coefficients.y * u + color.coefficients.z * v,
            y + color.coefficients.w * u
        );
    }
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if color.output.x > 0.5 {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;
