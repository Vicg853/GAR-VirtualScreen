use crate::resource_lifecycle::{
    InputEpoch, LatestReceiver, LatestSender, ReleaseGuard, latest_channel, release_barrier,
};
use crate::stream_engine::{DecodeApi, GpuPicture, H264, SoftwareH264, StreamEngine, VulkanH264};
use anyhow::{Context, Result, anyhow, bail};
use bridge_core::{
    diagnostics::Diagnostics,
    telemetry::{Outcome, Stage, TraceToken, now_us},
};
use bytemuck::{Pod, Zeroable};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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
    pub presented: u64,
    pub gpu_memory_bytes: u64,
    pub gpu_in_flight: u32,
    pub frame_age_us: Option<u64>,
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
                        #[cfg(target_os = "linux")]
                        winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(
                            &mut builder,
                            true,
                        );
                        #[cfg(target_os = "linux")]
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

    pub async fn submit(
        &self,
        bytes: Vec<u8>,
        keyframe: bool,
        sequence: u64,
        timestamp_us: u64,
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
            timestamp_us,
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
        timestamp_us: u64,
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
            timestamp_us,
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
        timestamp_us: u64,
        trace: Option<TraceToken>,
        queued_us: u64,
    },
}

struct DecodedOutput {
    frames: LatestSender<DecodedFrame>,
    control: mpsc::SyncSender<DecodedFrame>,
}

enum DecodedFrame {
    Release(mpsc::Sender<std::result::Result<(), String>>),
    Video(GpuPicture, Option<TraceToken>, u64),
}

enum EngineStart {
    Vulkan(Arc<gpu_video::VulkanDevice>),
    Software(wgpu::Device, wgpu::Queue),
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
    decoded_rx: Option<LatestReceiver<DecodedFrame>>,
    release_rx: Option<mpsc::Receiver<DecodedFrame>>,
    initialized: bool,
    instrumented: bool,
    prepare: fn(&mut Renderer, DecodedFrame) -> Result<PresentedFrame>,
    present: fn(&mut Renderer, Option<&PresentedFrame>) -> Result<bool>,
    collect: fn(&mut VideoApp),
    record_present: fn(&mut VideoApp),
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
            release_rx: None,
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
            record_present: if instrumented {
                Self::record_present::<true>
            } else {
                Self::record_present::<false>
            },
            present: if instrumented {
                Renderer::render::<true, false>
            } else {
                Renderer::render::<false, false>
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

        let (renderer, adapter_name, engine_start) = match initialize_wgpu_renderer(
            &window,
            self.instrumented,
        ) {
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
        self.diagnostics.record("renderer", format!("Vulkan presentation on {adapter_name}; decoded pixels stay on GPU after hardware decode or one software upload"));
        if self.instrumented
            && renderer
                .device
                .features()
                .contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            self.present = Renderer::render::<true, true>;
        }
        self.renderer = Some(renderer);

        let (decoder_tx, decoder_rx) = mpsc::sync_channel(1);
        let (frames, decoded_rx) = latest_channel();
        let (control, release_rx) = mpsc::sync_channel(1);
        let decoded_tx = DecodedOutput { frames, control };
        self.release_rx = Some(release_rx);
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
                        engine_start,
                    )
                } else {
                    decode_loop::<false>(
                        decoder_rx,
                        decoded_tx,
                        target,
                        diagnostics,
                        proxy,
                        released,
                        engine_start,
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

    fn record_present<const DEBUG: bool>(&mut self) {
        if let Some(frame) = &self.latest_frame {
            if !frame.presented.swap(true, Ordering::AcqRel) {
                if let Ok(mut status) = self.target.lock() {
                    status.presented += 1;
                    if DEBUG {
                        status.frame_age_us = frame.trace.as_ref().and_then(|t| {
                            (t.uncertainty_us != Some(u64::MAX)).then(|| {
                                (now_us() as i128 + t.offset_us as i128 - t.capture_us as i128)
                                    .max(0) as u64
                            })
                        });
                    }
                }
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
                status.presented = 0;
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
        }
        self.latest_frame = None;
        Ok(())
    }

    fn collect_decoded<const DEBUG: bool>(&mut self) {
        let mut latest = None;
        while let Some(decoded) = self
            .release_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
            .or_else(|| self.decoded_rx.as_ref().and_then(|rx| rx.take()))
        {
            if let DecodedFrame::Release(ack) = decoded {
                drop(latest.take());
                drop(self.decoded_rx.as_ref().and_then(|rx| rx.take()));
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
                            Ok(frame) => {
                                if DEBUG {
                                    if let Some(old) = &self.latest_frame {
                                        if !old.presented.load(Ordering::Acquire) {
                                            if let Some(t) = &old.trace {
                                                t.mark(Stage::Submit, now_us(), Some(now_us()), Outcome::Dropped,
                                                    "Prepared candidate replaced before presentation");
                                            }
                                        }
                                    }
                                }
                                self.latest_frame = Some(frame);
                            }
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
        let mut outstanding = false;
        for app in self.apps.values() {
            if let Some(renderer) = &app.renderer {
                let _ = renderer.device.poll(wgpu::PollType::Poll);
                outstanding |= renderer.in_flight.load(Ordering::Acquire) != 0
                    || renderer.query_in_flight.load(Ordering::Acquire) != 0;
                if let Ok(mut status) = app.target.lock() {
                    status.gpu_in_flight = renderer.in_flight.load(Ordering::Acquire) as u32;
                }
                if renderer.redraw_pending && renderer.in_flight.load(Ordering::Acquire) < 2 {
                    if let Some(window) = &app.window {
                        window.request_redraw();
                    }
                }
            }
        }
        if outstanding {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + Duration::from_millis(2),
            ));
            return;
        }
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
                    "Display closed by the viewer; detaching this viewer from its host screen",
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
                    match (app.present)(renderer, app.latest_frame.as_ref()) {
                        Ok(true) => (app.record_present)(app),
                        Ok(false) => {}
                        Err(error) => {
                            app.report_error(format!("Video presentation failed: {error:#}"))
                        }
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
        self.release_rx = None;
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

fn initialize_wgpu_renderer(
    window: &Arc<Window>,
    instrumented: bool,
) -> Result<(Renderer, String, EngineStart)> {
    // The library creates one Vulkan device for both decode and presentation.
    // A decoder on a different physical adapter is never silently paired here.
    let native = (|| -> Result<_> {
        if std::env::var("AIRBRIDGE_DECODER").is_ok_and(|value| value == "software") {
            bail!("Software engine selected by AIRBRIDGE_DECODER");
        }
        let instance = gpu_video::VulkanInstance::new()?;
        let surface = instance.wgpu_instance().create_surface(window.clone())?;
        let mut selected = None;
        let mut rejections = Vec::new();
        for adapter in instance.iter_adapters()? {
            if !adapter.supports_decoding() || !adapter.supports_surface(&surface) {
                continue;
            }
            let features = if instrumented
                && adapter
                    .wgpu_features()
                    .contains(wgpu::Features::TIMESTAMP_QUERY)
            {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            };
            match adapter.create_device(&gpu_video::parameters::VulkanDeviceDescriptor {
                wgpu_features: features,
                ..Default::default()
            }) {
                Ok(device) => {
                    selected = Some(device);
                    break;
                }
                Err(error) => {
                    if rejections.len() < 8 {
                        rejections.push(error.to_string());
                    }
                }
            }
        }
        let device = selected.ok_or_else(|| {
            anyhow::anyhow!(
                "No compatible Vulkan Video/presentation device: {}",
                rejections.join("; ")
            )
        })?;
        let adapter = device.wgpu_adapter();
        let name = adapter.get_info().name;
        let renderer = Renderer::new(
            surface,
            adapter,
            device.wgpu_device(),
            device.wgpu_queue(),
            window.inner_size(),
            window.clone(),
        )?;
        Ok((renderer, name, EngineStart::Vulkan(device)))
    })();
    match native {
        Ok(ready) => return Ok(ready),
        Err(error) => {
            if std::env::var("AIRBRIDGE_DECODER").is_ok_and(|value| value == "vulkan") {
                return Err(error.context("Forced Vulkan Video engine unavailable"));
            }
            // Startup-only fallback. This reason is forwarded through worker status.
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::VULKAN,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            });
            let surface = instance.create_surface(window.clone())?;
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    ..Default::default()
                }))
                .context("No Vulkan presentation adapter")?;
            let name = format!(
                "{}; native Vulkan Video unavailable: {error:#}; direct software decoding",
                adapter.get_info().name
            );
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("AirBridge software decode / Vulkan presentation"),
                    required_features: if instrumented
                        && adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY)
                    {
                        wgpu::Features::TIMESTAMP_QUERY
                    } else {
                        wgpu::Features::empty()
                    },
                    ..Default::default()
                }))?;
            let start = EngineStart::Software(device.clone(), queue.clone());
            Ok((
                Renderer::new(
                    surface,
                    adapter,
                    device,
                    queue,
                    window.inner_size(),
                    window.clone(),
                )?,
                name,
                start,
            ))
        }
    }
}

fn decode_loop<const DEBUG: bool>(
    receiver: mpsc::Receiver<VideoInput>,
    output: DecodedOutput,
    target: Arc<Mutex<VideoFrame>>,
    diagnostics: Arc<Diagnostics>,
    proxy: EventLoopProxy<VideoEvent>,
    released: mpsc::Receiver<()>,
    start: EngineStart,
) {
    // One dispatch per engine lifetime. Generic loops have no backend selector.
    match start {
        EngineStart::Vulkan(device) => match VulkanH264::new(device) {
            Ok(api) => {
                run_engine::<_, DEBUG>(api, receiver, output, target, diagnostics, proxy, released)
            }
            Err(error) => {
                record_error(&target, format!("Vulkan Video initialization: {error:#}"));
                drop(DecoderExit { target, proxy });
            }
        },
        EngineStart::Software(device, queue) => match SoftwareH264::new(device, queue) {
            Ok(api) => {
                run_engine::<_, DEBUG>(api, receiver, output, target, diagnostics, proxy, released)
            }
            Err(error) => {
                record_error(
                    &target,
                    format!("Software decoder initialization: {error:#}"),
                );
                drop(DecoderExit { target, proxy });
            }
        },
    }
}

fn run_engine<A: DecodeApi<H264>, const DEBUG: bool>(
    api: A,
    receiver: mpsc::Receiver<VideoInput>,
    output: DecodedOutput,
    target: Arc<Mutex<VideoFrame>>,
    diagnostics: Arc<Diagnostics>,
    proxy: EventLoopProxy<VideoEvent>,
    released: mpsc::Receiver<()>,
) {
    let mut engine = StreamEngine::<H264, A, DEBUG>::new(api);
    let exit = DecoderExit {
        target: target.clone(),
        proxy: proxy.clone(),
    };
    // Declared after engine: consumers and GPU reads release before decoder drop,
    // including early returns and unwinding. Parent bounds driver shutdown time.
    let _release_guard = ReleaseGuard::new(released, move || drop(exit));
    diagnostics.record("decoder", engine.api.name());
    update_decoder_status(&target, engine.api.name());
    let mut pending = HashMap::<u64, (TraceToken, u64)>::new();
    let mut active_epoch = 0;
    let mut recovery_required = true;
    let mut last_progress = std::time::Instant::now();
    let result = (|| -> Result<()> {
        loop {
            let pictures = engine.api.poll()?;
            if let Ok(mut status) = target.lock() {
                status.gpu_memory_bytes = engine.api.memory_bytes();
            }
            if !pictures.is_empty() {
                last_progress = std::time::Instant::now();
            }
            for picture in pictures {
                let trace = if DEBUG {
                    pending.remove(&picture.pts).map(|(token, start)| {
                        token.mark(Stage::Decode, start, Some(now_us()), Outcome::Complete,
                            "Decode/output availability observed; includes asynchronous GPU copy completion for hardware");
                        token
                    })
                } else {
                    None
                };
                if !publish_frame::<DEBUG>(
                    DecodedFrame::Video(picture, trace, if DEBUG { now_us() } else { 0 }),
                    &output,
                    &target,
                ) {
                    return Ok(());
                }
                let _ = proxy.send_event(VideoEvent::Wake);
            }
            if engine.api.pending_work() && last_progress.elapsed() > Duration::from_secs(5) {
                bail!("GPU engine made no progress for five seconds");
            }
            if !engine.api.pending_work() {
                last_progress = std::time::Instant::now();
            }
            if !engine.api.capacity() {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            let input = match receiver.recv_timeout(Duration::from_millis(2)) {
                Ok(input) => input,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            match input {
                VideoInput::Reset {
                    epoch,
                    width,
                    height,
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
                                "Engine generation reset",
                            );
                        }
                    }
                    if !release_decoder_frames(&output, &proxy, &target) {
                        return Ok(());
                    }
                    engine.api.reset()?;
                    recovery_required = true;
                    if let Ok(mut status) = target.lock() {
                        status.width = width;
                        status.height = height;
                        status.frames = 0;
                        status.presented = 0;
                        status.skipped = 0;
                    }
                }
                VideoInput::Packet {
                    epoch,
                    bytes,
                    keyframe,
                    sequence: _,
                    timestamp_us,
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
                                    "Obsolete generation",
                                );
                            }
                        }
                        continue;
                    }
                    if recovery_required && !keyframe {
                        if DEBUG {
                            if let Some(t) = trace {
                                t.mark(
                                    Stage::Decode,
                                    now_us(),
                                    Some(now_us()),
                                    Outcome::Dropped,
                                    "Waiting for recovery picture",
                                );
                            }
                        }
                        continue;
                    }
                    recovery_required = false;
                    let start = if DEBUG { now_us() } else { 0 };
                    if DEBUG {
                        if let Some(t) = &trace {
                            t.mark(
                                Stage::DecodeQueue,
                                queued_us,
                                Some(start),
                                Outcome::Complete,
                                "Bounded engine admission",
                            );
                            t.mark(
                                Stage::Decode,
                                start,
                                None,
                                Outcome::Pending,
                                "Awaiting decoded output",
                            );
                            if pending.len() >= 64 {
                                if let Some(old) = pending.keys().min().copied() {
                                    if let Some((t, began)) = pending.remove(&old) {
                                        t.mark(
                                            Stage::Decode,
                                            began,
                                            Some(now_us()),
                                            Outcome::Dropped,
                                            "Correlation capacity exceeded",
                                        );
                                    }
                                }
                            }
                            pending.insert(timestamp_us, (t.clone(), start));
                        }
                    }
                    if DEBUG {
                        engine.api.submit_instrumented(
                            &bytes,
                            timestamp_us,
                            keyframe,
                            trace.as_ref(),
                        )?;
                    } else {
                        engine.api.submit(&bytes, timestamp_us, keyframe)?;
                    }
                    // Wake display loop to drive GPU completion and input admission,
                    // even if decoding has not produced an output picture yet.
                    let _ = proxy.send_event(VideoEvent::Wake);
                }
            }
        }
    })();
    if let Err(error) = &result {
        diagnostics.record(
            "stream-engine",
            format!("{} failed: {error:#}", engine.api.name()),
        );
        record_error(&target, format!("{}: {error:#}", engine.api.name()));
    }
    if DEBUG {
        for (_, (t, start)) in pending {
            t.mark(
                Stage::Decode,
                start,
                Some(now_us()),
                if result.is_err() {
                    Outcome::Failed
                } else {
                    Outcome::Cancelled
                },
                if result.is_err() {
                    "Engine failed before output completion"
                } else {
                    "Stream engine stopped"
                },
            );
        }
    }
}

// This is a protocol barrier, not a delay: the renderer acknowledges only
// after GPU completion and release of imported/queued frame resources.
fn release_decoder_frames(
    output: &DecodedOutput,
    proxy: &EventLoopProxy<VideoEvent>,
    target: &Arc<Mutex<VideoFrame>>,
) -> bool {
    match release_barrier(&output.control, DecodedFrame::Release, || {
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
    output: &DecodedOutput,
    target: &Arc<Mutex<VideoFrame>>,
) -> bool {
    let (width, height) = match &frame {
        DecodedFrame::Video(frame, _, _) => (frame.width, frame.height),
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
    match output.frames.replace(frame) {
        Ok(displaced) => {
            if let Some(displaced) = displaced {
                if DEBUG {
                    if let DecodedFrame::Video(_, Some(t), queued_us) = &displaced {
                        t.mark(
                            Stage::RenderQueue,
                            *queued_us,
                            Some(now_us()),
                            Outcome::Dropped,
                            "Superseded by a newer decoded presentation candidate",
                        );
                    }
                }
                if let Ok(mut status) = target.lock() {
                    status.skipped += 1;
                }
                // Display candidates own independent output storage. Decoder references
                // remain owned by the backend and are never overwritten here.
                drop(displaced);
            }
            true
        }
        Err(_) => false,
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
    storage: Arc<crate::stream_engine::PictureStorage>,
    presented: AtomicBool,
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

    size: PhysicalSize<u32>,
    in_flight: Arc<AtomicUsize>,
    redraw_pending: bool,
    query_in_flight: Arc<AtomicUsize>,
    window: Arc<Window>,
}

impl Renderer {
    fn new(
        surface: wgpu::Surface<'static>,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        size: PhysicalSize<u32>,
        window: Arc<Window>,
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
            present_mode: if capabilities
                .present_modes
                .contains(&wgpu::PresentMode::Mailbox)
            {
                wgpu::PresentMode::Mailbox
            } else {
                wgpu::PresentMode::Fifo
            },
            alpha_mode: capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &configuration);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("AirBridge decoded-frame layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(4),
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
        Ok(Self {
            surface,
            device,
            queue,
            configuration,
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buffer,

            size,
            in_flight: Arc::new(AtomicUsize::new(0)),
            redraw_pending: false,
            query_in_flight: Arc::new(AtomicUsize::new(0)),
            window,
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
        let DecodedFrame::Video(frame, trace, queued_us) = decoded else {
            bail!("Release barrier is not a picture");
        };
        if DEBUG {
            if let Some(t) = &trace {
                t.mark(
                    Stage::RenderQueue,
                    queued_us,
                    Some(upload_start),
                    Outcome::Complete,
                    "GPU candidate queue; no pixel transfer",
                );
                t.mark(
                    Stage::Upload,
                    upload_start,
                    None,
                    Outcome::Pending,
                    "GPU views / bind group preparation",
                );
            }
            token = trace;
        }
        let width = frame.width;
        let height = frame.height;
        let storage = frame.storage;
        let color = ColorUniform {
            values: [
                if frame.color.full_range {
                    0.0
                } else {
                    -16.0 / 255.0
                },
                if frame.color.full_range {
                    1.0
                } else {
                    255.0 / 219.0
                },
                if frame.color.full_range {
                    1.0
                } else {
                    255.0 / 224.0
                },
                if frame.planar { 2.0 } else { 0.0 },
            ],
            coefficients: frame.color.coefficients(),
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
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&color));
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("AirBridge current video frame"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&storage.views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&storage.views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&storage.views[1]),
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
                    "GPU view/bind-group preparation; decoded pixels already resident",
                );
                t.mark(Stage::Submit, now_us(), None, Outcome::Pending, "");
            }
        }
        Ok(PresentedFrame {
            storage,
            presented: AtomicBool::new(false),
            bind_group,
            width,
            height,
            trace: token,
            measured: AtomicBool::new(false),
        })
    }

    fn render<const DEBUG: bool, const GPU_TIMESTAMPS: bool>(
        &mut self,
        frame: Option<&PresentedFrame>,
    ) -> Result<bool> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(false);
        }
        self.device.poll(wgpu::PollType::Poll)?;
        if self.in_flight.load(Ordering::Acquire) >= 2 {
            self.redraw_pending = true;
            return Ok(false);
        }
        self.redraw_pending = false;
        let acquire_start = if DEBUG { now_us() } else { 0 };
        let (output, reconfigure) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(anyhow!(
                    "GPU reported a validation error while acquiring the display surface"
                ));
            }
        };
        if DEBUG {
            if let Some(t) = frame
                .and_then(|f| f.trace.as_ref())
                .filter(|_| frame.is_some_and(|f| !f.measured.load(Ordering::Relaxed)))
            {
                t.mark(
                    Stage::SurfaceAcquire,
                    acquire_start,
                    Some(now_us()),
                    Outcome::Complete,
                    "Swapchain acquisition / compositor backpressure",
                );
            }
        }
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("AirBridge display frame"),
            });
        let queries = if GPU_TIMESTAMPS
            && self.query_in_flight.load(Ordering::Acquire) < 2
            && frame.is_some_and(|f| !f.measured.load(Ordering::Relaxed) && f.trace.is_some())
        {
            self.query_in_flight.fetch_add(1, Ordering::AcqRel);
            Some(self.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("AirBridge GPU render timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: 2,
            }))
        } else {
            None
        };
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
                timestamp_writes: queries.as_ref().map(|query_set| {
                    wgpu::RenderPassTimestampWrites {
                        query_set,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }
                }),
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
        let timestamp_readback = if let Some(queries) = &queries {
            let resolve = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("AirBridge GPU query resolve"),
                size: 256,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("AirBridge GPU query readback"),
                size: 16,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.resolve_query_set(queries, 0..2, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, 16);
            Some(readback)
        } else {
            None
        };
        let token = if DEBUG {
            frame
                .filter(|f| !f.measured.swap(true, Ordering::Relaxed))
                .and_then(|f| f.trace.as_ref())
        } else {
            None
        };
        let submit_start = if DEBUG { now_us() } else { 0 };
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.queue.submit(Some(encoder.finish()));
        let resources = frame.map(|f| f.storage.clone());
        let in_flight = self.in_flight.clone();
        self.queue.on_submitted_work_done(move || {
            drop(resources); // Imported/decoded storage cannot die before GPU reads finish.
            in_flight.fetch_sub(1, Ordering::AcqRel);
        });
        if DEBUG {
            if let Some(t) = token {
                if let Some(readback) = timestamp_readback {
                    let mapped = readback.clone();
                    let count = self.query_in_flight.clone();
                    let t = t.clone();
                    let period = self.queue.get_timestamp_period() as f64;
                    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                        if result.is_ok() {
                            let Ok(data) = mapped.slice(..).get_mapped_range() else {
                                t.mark(Stage::GpuRender, submit_start, None, Outcome::Failed, "GPU query mapping invalidated");
                                count.fetch_sub(1, Ordering::AcqRel);
                                return;
                            };
                            let begin = u64::from_le_bytes(data[..8].try_into().unwrap());
                            let end = u64::from_le_bytes(data[8..16].try_into().unwrap());
                            if let Some(ticks) = end.checked_sub(begin) {
                                let duration = (ticks as f64 * period / 1000.) as u64;
                                t.mark(Stage::GpuRender, submit_start, Some(submit_start.saturating_add(duration)), Outcome::Complete,
                                    "GPU timestamp duration; stage start anchored to CPU submission, not calibrated GPU start");
                            } else { t.mark(Stage::GpuRender, submit_start, None, Outcome::Unavailable, "GPU timestamps wrapped or invalid"); }
                            drop(data); mapped.unmap();
                        } else { t.mark(Stage::GpuRender, submit_start, None, Outcome::Failed, "GPU timestamp readback failed"); }
                        count.fetch_sub(1, Ordering::AcqRel);
                    });
                } else {
                    t.mark(
                        Stage::GpuRender,
                        submit_start,
                        None,
                        Outcome::Unavailable,
                        "GPU timestamp queries unavailable or bounded query slots occupied",
                    );
                }
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
        self.window.pre_present_notify();
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
        Ok(true)
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
@group(0) @binding(4) var plane_v: texture_2d<f32>;
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
    if color.values.w > 0.5 && color.values.w < 1.5 {
        rgb = textureSample(plane_y_or_rgba, frame_sampler, input.uv).rgb;
    } else {
        let luma = textureSample(plane_y_or_rgba, frame_sampler, input.uv).r;
        var chroma = textureSample(plane_uv, frame_sampler, input.uv).rg;
        if color.values.w > 1.5 { chroma.y = textureSample(plane_v, frame_sampler, input.uv).r; }
        chroma -= vec2<f32>(128.0/255.0, 128.0/255.0);
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

#[cfg(test)]
mod shader_tests {
    #[test]
    fn direct_yuv_shader_is_valid_without_opening_a_window() {
        let module = naga::front::wgsl::parse_str(super::VIDEO_SHADER).unwrap();
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
    }
}
