use crate::resource_lifecycle::{InputEpoch, ReleaseGuard, release_barrier};
use anyhow::{Context, Result, anyhow, bail};
use bridge_core::{
    diagnostics::Diagnostics,
    telemetry::{Outcome, Stage, TraceToken, now_us},
};
use bytemuck::{Pod, Zeroable};
use hang::catalog::{H264, VideoConfig};
use moq_net::Timestamp;
use moq_video::{Frame as DecodedVideoFrame, decode, render};
use render::wgpu;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
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
    pub closed: bool,
    pub user_closed: bool,
}

static DISPLAY_THREAD: OnceLock<Mutex<Option<thread::JoinHandle<()>>>> = OnceLock::new();
static DISPLAY_PROXY: OnceLock<std::result::Result<EventLoopProxy<VideoEvent>, String>> =
    OnceLock::new();

struct WindowLease {
    proxy: EventLoopProxy<VideoEvent>,
    id: u64,
}
impl Drop for WindowLease {
    fn drop(&mut self) {
        let _ = self.proxy.send_event(VideoEvent::Close(self.id, None));
    }
}

#[derive(Clone)]
pub struct VideoWindow {
    input: async_mpsc::Sender<VideoInput>,
    epoch: InputEpoch,
    proxy: Option<EventLoopProxy<VideoEvent>>,
    unavailable: Option<String>,
    id: u64,
    _lease: Option<Arc<WindowLease>>,
}

impl VideoWindow {
    pub fn spawn(target: Arc<Mutex<VideoFrame>>, diagnostics: Arc<Diagnostics>) -> Result<Self> {
        Self::spawn_mode(target, diagnostics, false)
    }
    pub fn spawn_mode(
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
        instrumented: bool,
    ) -> Result<Self> {
        // Winit permits one event loop per process. All independent displays
        // share that event loop while keeping their own renderer and decoder.
        static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);
        let proxy = DISPLAY_PROXY
            .get_or_init(|| {
                let (send, receive) = mpsc::sync_channel(1);
                let worker = thread::Builder::new()
                    .name("airbridge-display-events".into())
                    .spawn(move || {
                        let mut builder = EventLoop::<VideoEvent>::with_user_event();
                        winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(
                            &mut builder,
                            true,
                        );
                        winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(
                            &mut builder,
                            true,
                        );
                        let event_loop = match builder.build() {
                            Ok(event_loop) => event_loop,
                            Err(error) => {
                                let _ = send.send(Err(format!(
                                    "Cannot initialize display events: {error}"
                                )));
                                return;
                            }
                        };
                        let proxy = event_loop.create_proxy();
                        if send.send(Ok(proxy.clone())).is_err() {
                            return;
                        }
                        let mut manager = DisplayManager {
                            apps: HashMap::new(),
                            proxy,
                            diagnostic_ids: HashSet::new(),
                            wait: DisplayManager::wait::<false>,
                        };
                        if let Err(error) = event_loop.run_app(&mut manager) {
                            for app in manager.apps.values() {
                                app.report_error(format!("Display event loop stopped: {error}"));
                            }
                        }
                    })
                    .map_err(|error| format!("Cannot start display events: {error}"))?;
                let _ = DISPLAY_THREAD.set(Mutex::new(Some(worker)));
                receive
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|error| format!("Display event startup failed: {error}"))?
            })
            .as_ref()
            .map_err(|error| anyhow!("{error}"))?
            .clone();
        let (input, input_rx) = async_mpsc::channel(1);
        let id = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
        proxy
            .send_event(VideoEvent::Create {
                id,
                input_rx,
                target,
                diagnostics,
                instrumented,
            })
            .map_err(|_| anyhow!("The display event loop has stopped"))?;
        Ok(Self {
            input,
            epoch: InputEpoch::default(),
            proxy: Some(proxy.clone()),
            unavailable: None,
            id,
            _lease: Some(Arc::new(WindowLease { proxy, id })),
        })
    }

    pub fn unavailable(message: String, target: Arc<Mutex<VideoFrame>>) -> Self {
        let (input, receiver) = async_mpsc::channel(1);
        drop(receiver);
        record_error(&target, message.clone());
        Self {
            input,
            epoch: InputEpoch::default(),
            proxy: None,
            unavailable: Some(message),
            id: 0,
            _lease: None,
        }
    }

    pub fn shutdown_backend() {
        if let Some(Ok(proxy)) = DISPLAY_PROXY.get() {
            let _ = proxy.send_event(VideoEvent::Shutdown);
        }
        // Application return must not terminate the process while native
        // decoder or GPU destruction is still running on the event thread.
        if let Some(worker) = DISPLAY_THREAD
            .get()
            .and_then(|slot| slot.lock().ok().and_then(|mut slot| slot.take()))
        {
            let _ = worker.join();
        }
    }

    pub fn show(&self) -> Result<()> {
        self.send_event(VideoEvent::Show(self.id))
    }

    pub fn close(&self) -> Result<()> {
        self.epoch.invalidate();
        self.send_event(VideoEvent::Close(self.id, None))
    }

    pub fn close_wait(&self) -> Result<()> {
        self.epoch.invalidate();
        let (ack, closed) = mpsc::channel();
        self.send_event(VideoEvent::Close(self.id, Some(ack)))?;
        closed
            .recv()
            .context("Native display cleanup acknowledgement cancelled")
    }
    pub async fn reset(&mut self, width: u32, height: u32) -> Result<()> {
        let epoch = self.epoch.advance();
        self.send_input(VideoInput::Reset {
            width,
            height,
            epoch,
        })
        .await
    }

    pub async fn submit(&self, bytes: Vec<u8>, keyframe: bool) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_QUEUED_PACKET_BYTES {
            bail!("Video packet size is outside the supported range");
        }
        let epoch = self
            .epoch
            .current()
            .context("Video producer belongs to a replaced or closed stream")?;
        self.send_input(VideoInput::Packet {
            epoch,
            bytes,
            keyframe,
            sequence: 0,
            trace: None,
            queued_us: 0,
        })
        .await
    }

    pub async fn submit_debug(
        &self,
        bytes: Vec<u8>,
        keyframe: bool,
        sequence: u64,
        trace: Option<TraceToken>,
    ) -> Result<()> {
        if bytes.is_empty() || bytes.len() > MAX_QUEUED_PACKET_BYTES {
            bail!("Video packet size is outside the supported range");
        }
        let epoch = self
            .epoch
            .current()
            .context("Video producer belongs to a replaced or closed stream")?;
        self.send_input(VideoInput::Packet {
            epoch,
            bytes,
            keyframe,
            sequence,
            trace,
            queued_us: now_us(),
        })
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
    Create {
        id: u64,
        input_rx: async_mpsc::Receiver<VideoInput>,
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
        instrumented: bool,
    },
    Wake,
    Show(u64),
    Close(u64, Option<mpsc::Sender<()>>),
    Shutdown,
}

#[derive(Debug)]
enum VideoInput {
    Reset {
        epoch: u64,
        width: u32,
        height: u32,
    },
    Packet {
        epoch: u64,
        bytes: Vec<u8>,
        keyframe: bool,
        sequence: u64,
        trace: Option<TraceToken>,
        queued_us: u64,
    },
}

enum DecodedFrame {
    Release(mpsc::Sender<std::result::Result<(), String>>),
    Video(DecodedVideoFrame, Option<TraceToken>, u64),
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
    decoder_worker: Option<thread::JoinHandle<()>>,
    decoder_release: Option<mpsc::Sender<()>>,
    decoded_rx: Option<mpsc::Receiver<DecodedFrame>>,
    initialized: bool,
    instrumented: bool,
    prepare: fn(&mut Renderer, DecodedFrame) -> Result<PresentedFrame>,
    present: fn(&mut Renderer, Option<&PresentedFrame>) -> Result<()>,
    collect: fn(&mut VideoApp),
}

impl VideoApp {
    fn new(
        input_rx: async_mpsc::Receiver<VideoInput>,
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
        proxy: EventLoopProxy<VideoEvent>,
        instrumented: bool,
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
            decoder_worker: None,
            decoder_release: None,
            decoded_rx: None,
            initialized: false,
            instrumented,
            prepare: if instrumented {
                Renderer::set_frame::<true>
            } else {
                Renderer::set_frame::<false>
            },
            collect: if instrumented {
                VideoApp::collect_decoded::<true>
            } else {
                VideoApp::collect_decoded::<false>
            },
            present: if instrumented {
                Renderer::render::<true>
            } else {
                Renderer::render::<false>
            },
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
        let instrumented = self.instrumented;
        let (release, released) = mpsc::channel();
        self.decoder_release = Some(release);
        match thread::Builder::new()
            .name("airbridge-video-decoder".into())
            .spawn(move || {
                if instrumented {
                    decode_loop::<true>(
                        decoder_rx,
                        decoded_tx,
                        target,
                        diagnostics,
                        proxy,
                        released,
                    )
                } else {
                    decode_loop::<false>(
                        decoder_rx,
                        decoded_tx,
                        target,
                        diagnostics,
                        proxy,
                        released,
                    )
                }
            }) {
            Ok(worker) => self.decoder_worker = Some(worker),
            Err(error) => {
                self.report_error(format!("Cannot start the video decoder worker: {error}"));
                self.decoder_tx = None;
                self.decoder_release = None;
            }
        }
    }

    fn report_error(&self, message: String) {
        self.diagnostics.record("video-window", &message);
        record_error(&self.target, message);
        if let Ok(mut target) = self.target.lock() {
            target.closed = true;
        }
    }

    fn pump_input(&mut self) {
        if self.pending_input.is_none() {
            self.pending_input = self.input_rx.try_recv().ok();
        }
        let Some(input) = self.pending_input.take() else {
            return;
        };
        if let VideoInput::Reset { width, height, .. } = &input {
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

    fn release_frames(&mut self) -> Result<()> {
        if let Some(renderer) = &mut self.renderer {
            renderer.quiesce()?;
            // Drop importer leases only after all submitted reads are complete.
            renderer.frame_renderer = None;
        }
        self.latest_frame = None;
        Ok(())
    }

    fn collect_decoded<const DEBUG: bool>(&mut self) {
        let mut latest = None;
        while let Some(decoded) = self.decoded_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            if let DecodedFrame::Release(ack) = decoded {
                drop(latest.take());
                let result = self.release_frames().map_err(|e| format!("{e:#}"));
                if let Err(error) = &result {
                    self.report_error(format!(
                        "Could not release frames before decoder replacement: {error}"
                    ));
                }
                let _ = ack.send(result);
                continue;
            }
            if DEBUG {
                if let Some(DecodedFrame::Video(_, Some(t), queued_us)) = &latest {
                    t.mark(
                        Stage::RenderQueue,
                        *queued_us,
                        Some(now_us()),
                        Outcome::Dropped,
                        "Superseded before GPU preparation",
                    );
                }
            }
            latest = Some(decoded);
        }
        if let Some(decoded) = latest {
            match decoded {
                DecodedFrame::Release(_) => unreachable!("release barrier handled above"),
                decoded => {
                    let trace = if DEBUG {
                        match &decoded {
                            DecodedFrame::Video(_, t, _) => t.clone(),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some(renderer) = &mut self.renderer {
                        match (self.prepare)(renderer, decoded) {
                            Ok(frame) => self.latest_frame = Some(frame),
                            Err(error) => {
                                if DEBUG {
                                    if let Some(t) = trace {
                                        t.mark(
                                            Stage::Upload,
                                            now_us(),
                                            Some(now_us()),
                                            Outcome::Failed,
                                            &error.to_string(),
                                        );
                                    }
                                }
                                self.report_error(format!(
                                    "Could not prepare a decoded frame for display: {error:#}"
                                ));
                            }
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

struct DisplayManager {
    apps: HashMap<u64, VideoApp>,
    proxy: EventLoopProxy<VideoEvent>,
    diagnostic_ids: HashSet<u64>,
    wait: fn(&mut DisplayManager, &ActiveEventLoop),
}
impl DisplayManager {
    fn update_wait_policy(&mut self) {
        self.wait = if self.diagnostic_ids.is_empty() {
            Self::wait::<false>
        } else {
            Self::wait::<true>
        };
    }
    fn wait<const DEBUG: bool>(&mut self, event_loop: &ActiveEventLoop) {
        if DEBUG {
            for id in &self.diagnostic_ids {
                if let Some(renderer) = self.apps.get(id).and_then(|app| app.renderer.as_ref()) {
                    let _ = renderer.device.poll(wgpu::PollType::Poll);
                }
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + Duration::from_millis(10),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl ApplicationHandler<VideoEvent> for DisplayManager {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        for app in self.apps.values_mut() {
            app.initialize(event_loop);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: VideoEvent) {
        match event {
            VideoEvent::Create {
                id,
                input_rx,
                target,
                diagnostics,
                instrumented,
            } => {
                let mut app = VideoApp::new(
                    input_rx,
                    target,
                    diagnostics,
                    self.proxy.clone(),
                    instrumented,
                );
                app.initialize(event_loop);
                if app.renderer.is_some() && app.decoder_tx.is_some() {
                    self.apps.insert(id, app);
                    if instrumented {
                        self.diagnostic_ids.insert(id);
                        self.update_wait_policy();
                    }
                }
            }
            VideoEvent::Show(id) => {
                if let Some(window) = self.apps.get(&id).and_then(|app| app.window.as_ref()) {
                    window.set_visible(true);
                    window.focus_window();
                }
            }
            VideoEvent::Close(id, ack) => {
                self.apps.remove(&id);
                self.diagnostic_ids.remove(&id);
                self.update_wait_policy();
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            VideoEvent::Shutdown => {
                self.apps.clear();
                self.diagnostic_ids.clear();
                self.update_wait_policy();
                event_loop.exit();
            }
            VideoEvent::Wake => {}
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(id) = self
            .apps
            .iter()
            .find(|(_, app)| {
                app.window
                    .as_ref()
                    .is_some_and(|window| window.id() == window_id)
            })
            .map(|(id, _)| *id)
        else {
            return;
        };
        if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
            self.diagnostic_ids.remove(&id);
            self.update_wait_policy();
            if let Some(app) = self.apps.remove(&id) {
                if let Ok(mut target) = app.target.lock() {
                    target.user_closed = true;
                }
                app.diagnostics.record(
                    "video-window",
                    "Display closed by the viewer; removing its host screen",
                );
            }
            return;
        }
        let Some(app) = self.apps.get_mut(&id) else {
            return;
        };
        match event {
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut app.renderer {
                    renderer.resize(size);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if let (Some(renderer), Some(window)) = (&mut app.renderer, &app.window) {
                    renderer.resize(window.inner_size());
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut app.renderer {
                    if let Err(error) = (app.present)(renderer, app.latest_frame.as_ref()) {
                        app.report_error(format!("Video presentation failed: {error:#}"));
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let closed = self
            .apps
            .iter()
            .filter_map(|(id, app)| {
                app.target
                    .lock()
                    .map(|target| target.closed)
                    .unwrap_or(true)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in closed {
            self.apps.remove(&id);
            self.diagnostic_ids.remove(&id);
            self.update_wait_policy();
        }
        for app in self.apps.values_mut() {
            app.pump_input();
            (app.collect)(app);
        }
        (self.wait)(self, event_loop);
    }
}

impl Drop for VideoApp {
    fn drop(&mut self) {
        self.input_rx.close();
        self.pending_input = None;
        // Stop packet admission first. The worker retains its native backend
        // behind ReleaseGuard until every consumer below has been released.
        self.decoder_tx = None;
        if let Ok(mut target) = self.target.lock() {
            target.closed = true;
        }
        if let Err(error) = self.release_frames() {
            self.diagnostics
                .record("video-window", format!("GPU cleanup reported: {error:#}"));
        }
        // Disconnect output before joining: a producer waiting on a full queue
        // or a replacement acknowledgement must be able to wake and exit.
        self.decoded_rx = None;
        self.latest_frame = None;
        self.renderer = None;
        if let Some(release) = self.decoder_release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.decoder_worker.take() {
            if worker.join().is_err() {
                self.diagnostics
                    .record("video-window", "Decoder worker panicked during shutdown");
            }
        }
        self.window = None;
        self.diagnostics.record(
            "video-window",
            "GPU work finished; frames, decoder worker and display released",
        );
    }
}

// A fatal decoder exit wakes the window manager and closes the receiving lane.
struct DecoderExit {
    target: Arc<Mutex<VideoFrame>>,
    proxy: EventLoopProxy<VideoEvent>,
}
impl Drop for DecoderExit {
    fn drop(&mut self) {
        if let Ok(mut target) = self.target.lock() {
            if !target.closed && target.error.is_none() {
                target.error = Some("The decoder worker stopped".into());
            }
            target.closed = true;
        }
        let _ = self.proxy.send_event(VideoEvent::Wake);
    }
}

fn initialize_wgpu_renderer(window: &Arc<Window>) -> Result<(Renderer, String, bool)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let surface = instance
        .create_surface(window.clone())
        .context("Cannot create the GPU display surface")?;
    let adapter_options = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        ..Default::default()
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

fn decode_loop<const DEBUG: bool>(
    receiver: mpsc::Receiver<VideoInput>,
    output: mpsc::SyncSender<DecodedFrame>,
    target: Arc<Mutex<VideoFrame>>,
    diagnostics: Arc<Diagnostics>,
    proxy: EventLoopProxy<VideoEvent>,
    released: mpsc::Receiver<()>,
) {
    let exit = DecoderExit {
        target: target.clone(),
        proxy: proxy.clone(),
    };
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
    // Declared after backend: on every exit, consumers release first, then
    // the native decoder is destroyed on this same worker thread.
    let _release_guard = ReleaseGuard::new(released, move || drop(exit));
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
    let mut pending = HashMap::<u64, (TraceToken, u64)>::new();
    let mut active_epoch = 0;
    while let Ok(input) = receiver.recv() {
        match input {
            VideoInput::Reset {
                width,
                height,
                epoch,
            } => {
                if epoch <= active_epoch {
                    continue;
                }
                active_epoch = epoch;
                if DEBUG {
                    for (_, (t, start)) in pending.drain() {
                        t.mark(
                            Stage::Decode,
                            start,
                            Some(now_us()),
                            Outcome::Cancelled,
                            "Decoder reset",
                        );
                    }
                }
                packet_count = 0;
                error_count = 0;
                if let Ok(mut status) = target.lock() {
                    status.width = width;
                    status.height = height;
                    status.frames = 0;
                    status.skipped = 0;
                    status.error = None;
                }
                if !release_decoder_frames(&output, &proxy, &target) {
                    return;
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
            }
            VideoInput::Packet {
                epoch,
                bytes,
                keyframe,
                sequence,
                trace,
                queued_us,
            } => {
                if epoch != active_epoch {
                    if DEBUG {
                        if let Some(t) = trace {
                            t.mark(
                                Stage::DecodeQueue,
                                queued_us,
                                Some(now_us()),
                                Outcome::Cancelled,
                                "Packet belongs to an obsolete decoder generation",
                            );
                        }
                    }
                    continue;
                }
                let decode_start = if DEBUG { now_us() } else { 0 };
                if DEBUG {
                    if let Some(t) = &trace {
                        t.mark(
                            Stage::DecodeQueue,
                            queued_us,
                            Some(decode_start),
                            Outcome::Complete,
                            "",
                        );
                        t.mark(Stage::Decode, decode_start, None, Outcome::Pending, "");
                        if pending.len() >= 64 {
                            if let Some(id) = pending.keys().min().copied() {
                                if let Some((t, start)) = pending.remove(&id) {
                                    t.mark(
                                        Stage::Decode,
                                        start,
                                        Some(now_us()),
                                        Outcome::Dropped,
                                        "Decoder correlation capacity exceeded",
                                    );
                                }
                            }
                        }
                        pending.insert(sequence.saturating_add(1), (t.clone(), decode_start));
                    }
                }
                packet_count += 1;
                let timestamp = Timestamp::from_micros(if DEBUG {
                    sequence.saturating_add(1)
                } else {
                    packet_count
                })
                .unwrap_or(Timestamp::ZERO);
                let payload = bytes::Bytes::from(bytes);
                let frames = backend
                    .decoder
                    .decode(&payload, timestamp, keyframe)
                    .map(|frames| {
                        frames
                            .into_iter()
                            .map(|frame| {
                                let trace = if DEBUG {
                                    pending.remove(&(frame.timestamp.as_micros() as u64)).map(
                                        |(t, start)| {
                                            t.mark(
                                                Stage::Decode,
                                                start,
                                                Some(now_us()),
                                                Outcome::Complete,
                                                "",
                                            );
                                            t
                                        },
                                    )
                                } else {
                                    None
                                };
                                DecodedFrame::Video(frame, trace, if DEBUG { now_us() } else { 0 })
                            })
                            .collect::<Vec<_>>()
                    })
                    .map_err(|error| anyhow!(error));
                match frames {
                    Ok(frames) => {
                        for frame in frames {
                            if !publish_frame::<DEBUG>(frame, &output, &target) {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        if DEBUG {
                            if let Some((t, start)) = pending.remove(&sequence.saturating_add(1)) {
                                t.mark(
                                    Stage::Decode,
                                    start,
                                    Some(now_us()),
                                    Outcome::Failed,
                                    &error.to_string(),
                                );
                            }
                        }
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
                            if !release_decoder_frames(&output, &proxy, &target) {
                                return;
                            }
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
                                                if !publish_frame::<DEBUG>(
                                                    DecodedFrame::Video(frame, None, 0),
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
                                            if !release_decoder_frames(&output, &proxy, &target) {
                                                return;
                                            }
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
    if DEBUG {
        for (_, (t, start)) in pending {
            t.mark(
                Stage::Decode,
                start,
                Some(now_us()),
                Outcome::Cancelled,
                "Decoder worker stopped",
            );
        }
    }
}

// This is a protocol barrier, not a delay: the renderer acknowledges only
// after GPU completion and release of imported/queued frame resources.
fn release_decoder_frames(
    output: &mpsc::SyncSender<DecodedFrame>,
    proxy: &EventLoopProxy<VideoEvent>,
    target: &Arc<Mutex<VideoFrame>>,
) -> bool {
    match release_barrier(output, DecodedFrame::Release, || {
        let _ = proxy.send_event(VideoEvent::Wake);
    }) {
        Ok(()) => true,
        Err(error) => {
            if !target.lock().map(|t| t.closed).unwrap_or(true) {
                record_error(target, error);
            }
            false
        }
    }
}

fn publish_frame<const DEBUG: bool>(
    frame: DecodedFrame,
    output: &mpsc::SyncSender<DecodedFrame>,
    target: &Arc<Mutex<VideoFrame>>,
) -> bool {
    let (width, height) = match &frame {
        DecodedFrame::Video(frame, _, _) => (frame.size().width, frame.size().height),
        DecodedFrame::Release(_) => return true,
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
    let trace = if DEBUG {
        match &frame {
            DecodedFrame::Video(_, t, _) => t.clone(),
            _ => None,
        }
    } else {
        None
    };
    if DEBUG {
        if let Some(t) = &trace {
            t.mark(
                Stage::RenderQueue,
                now_us(),
                None,
                Outcome::Pending,
                "Awaiting GPU frame preparation",
            );
        }
    }
    match output.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(_)) => {
            if DEBUG {
                if let Some(t) = trace {
                    t.mark(
                        Stage::RenderQueue,
                        now_us(),
                        Some(now_us()),
                        Outcome::Dropped,
                        "Decoded frame queue full",
                    );
                }
            }
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
    trace: Option<TraceToken>,
    measured: AtomicBool,
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
    frame_renderer: Option<render::Renderer>,
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
            color_space: wgpu::SurfaceColorSpace::Auto,
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
            frame_renderer: Some(frame_renderer),
            size,
        })
    }

    fn quiesce(&self) -> Result<()> {
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .context("Waiting for GPU frame work during cleanup")?;
        Ok(())
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

    fn set_frame<const DEBUG: bool>(&mut self, decoded: DecodedFrame) -> Result<PresentedFrame> {
        let upload_start = if DEBUG { now_us() } else { 0 };
        let mut token = None;
        let (texture, y_view, uv_view, width, height, color) = match decoded {
            DecodedFrame::Video(frame, trace, queued_us) => {
                if DEBUG {
                    if let Some(t) = &trace {
                        t.mark(
                            Stage::RenderQueue,
                            queued_us,
                            Some(upload_start),
                            Outcome::Complete,
                            "",
                        );
                        t.mark(Stage::Upload, upload_start, None, Outcome::Pending, "");
                    }
                    token = trace;
                }
                let size = frame.size();
                if size.width == 0
                    || size.height == 0
                    || size.width > 7680
                    || size.height > 4320
                    || size.pixels() > MAX_FRAME_PIXELS
                {
                    bail!("Decoded frame exceeds the 7680×4320 presentation limit");
                }
                if self.frame_renderer.is_none() {
                    self.frame_renderer = Some(
                        render::Renderer::new(&self.device, &self.queue, render::Config::new())
                            .context("Recreating frame importer after decoder reset")?,
                    );
                }
                let rendered = self
                    .frame_renderer
                    .as_mut()
                    .context("Frame importer unavailable")?
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
            DecodedFrame::Release(_) => bail!("Release barrier is not a displayable video frame"),
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
        if DEBUG {
            if let Some(t) = &token {
                t.mark(
                    Stage::Upload,
                    upload_start,
                    Some(now_us()),
                    Outcome::Complete,
                    "CPU preparation and GPU import/upload submission, not GPU completion",
                );
                t.mark(Stage::Submit, now_us(), None, Outcome::Pending, "");
            }
        }
        Ok(PresentedFrame {
            _texture: texture,
            bind_group,
            width,
            height,
            trace: token,
            measured: AtomicBool::new(false),
        })
    }

    fn render<const DEBUG: bool>(&mut self, frame: Option<&PresentedFrame>) -> Result<()> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(());
        }
        let (output, reconfigure) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!(
                    "GPU reported a validation error while acquiring the display surface"
                ));
            }
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
        let token = if DEBUG {
            frame
                .filter(|f| !f.measured.swap(true, Ordering::Relaxed))
                .and_then(|f| f.trace.as_ref())
        } else {
            None
        };
        let submit_start = if DEBUG { now_us() } else { 0 };
        self.queue.submit(Some(encoder.finish()));
        if DEBUG {
            if let Some(t) = token {
                t.mark(
                    Stage::Submit,
                    submit_start,
                    Some(now_us()),
                    Outcome::Complete,
                    "",
                );
                let t = t.clone();
                let completion_start = now_us();
                t.mark(
                    Stage::GpuComplete,
                    completion_start,
                    None,
                    Outcome::Pending,
                    "",
                );
                self.queue.on_submitted_work_done(move || {
                    t.mark(
                        Stage::GpuComplete,
                        completion_start,
                        Some(now_us()),
                        Outcome::Complete,
                        "Callback observation includes driver scheduling delay",
                    )
                });
            }
        }
        let present_start = if DEBUG { now_us() } else { 0 };
        self.queue.present(output);
        if DEBUG {
            if let Some(t) = token {
                t.mark(
                    Stage::PresentSubmit,
                    present_start,
                    Some(now_us()),
                    Outcome::Complete,
                    "Queued to compositor; scanout not observed",
                );
            }
        }
        if DEBUG {
            let _ = self.device.poll(wgpu::PollType::Poll);
        }
        if reconfigure {
            self.surface.configure(&self.device, &self.configuration);
        }
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
