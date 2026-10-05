//! Parent-side supervisor. No codec, GPU or native window calls run here.
use crate::resource_lifecycle::InputEpoch;
use anyhow::{Context, Result, bail};
use bridge_core::{
    diagnostics::Diagnostics,
    telemetry::{ClockEstimate, Outcome, Sample, Stage, Telemetry, TraceEvent, TraceToken, now_us},
    worker::{self, Message, TraceSeed},
};
use std::{
    io::Read,
    net::Shutdown,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Child, Command},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

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
    pub cleanup_complete: bool,
}
struct Input {
    epoch: u64,
    message: Message,
    payload: Vec<u8>,
}
struct Owner {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Owner {
    fn join(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.lock().ok().and_then(|mut h| h.take()) {
            let _ = thread.join();
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.join();
    }
}
static WORKERS: OnceLock<Mutex<Vec<Weak<Owner>>>> = OnceLock::new();
#[derive(Clone)]
pub struct VideoWindow {
    input: mpsc::SyncSender<Input>,
    epoch: InputEpoch,
    owner: Arc<Owner>,
    target: Arc<Mutex<VideoFrame>>,
    telemetry: Arc<Mutex<Option<Arc<Telemetry>>>>,
    unavailable: Option<String>,
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
        let (tx, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let telemetry = Arc::new(Mutex::new(None));
        let child_target = target.clone();
        let child_stop = stop.clone();
        let child_telemetry = telemetry.clone();
        let thread = thread::Builder::new()
            .name("airbridge-native-supervisor".into())
            .spawn(move || {
                if instrumented {
                    supervise::<true>(rx, child_stop, child_target, child_telemetry, diagnostics);
                } else {
                    supervise::<false>(rx, child_stop, child_target, child_telemetry, diagnostics);
                }
            })?;
        let owner = Arc::new(Owner {
            stop,
            thread: Mutex::new(Some(thread)),
        });
        let registry = WORKERS.get_or_init(Default::default);
        if let Ok(mut workers) = registry.lock() {
            workers.retain(|w| w.strong_count() > 0);
            workers.push(Arc::downgrade(&owner));
        }
        Ok(Self {
            input: tx,
            epoch: InputEpoch::default(),
            owner,
            target,
            telemetry,
            unavailable: None,
        })
    }
    pub fn unavailable(message: String, target: Arc<Mutex<VideoFrame>>) -> Self {
        let (input, rx) = mpsc::sync_channel(1);
        drop(rx);
        if let Ok(mut s) = target.lock() {
            s.error = Some(message.clone());
            s.closed = true;
            s.cleanup_complete = true;
        }
        Self {
            input,
            epoch: InputEpoch::default(),
            owner: Arc::new(Owner {
                stop: Arc::new(AtomicBool::new(true)),
                thread: Mutex::new(None),
            }),
            target,
            telemetry: Arc::new(Mutex::new(None)),
            unavailable: Some(message),
        }
    }
    pub fn shutdown_backend() {
        let workers = WORKERS
            .get()
            .and_then(|w| {
                w.lock()
                    .ok()
                    .map(|w| w.iter().filter_map(Weak::upgrade).collect::<Vec<_>>())
            })
            .unwrap_or_default();
        for w in &workers {
            w.stop.store(true, Ordering::Release);
        }
        for w in workers {
            w.join();
        }
    }
    pub fn close(&self) -> Result<()> {
        self.epoch.invalidate();
        self.owner.stop.store(true, Ordering::Release);
        Ok(())
    }
    pub async fn close_wait(&self) -> Result<()> {
        self.close()?;
        let owner = self.owner.clone();
        tokio::task::spawn_blocking(move || owner.join())
            .await
            .context("Native worker cleanup task failed")?;
        Ok(())
    }
    pub fn show(&self) -> Result<()> {
        if self.owner.stop.load(Ordering::Acquire) {
            bail!("Display is stopped; retry the stream to reopen it");
        }
        self.input
            .try_send(Input {
                epoch: self.epoch.current().unwrap_or(0),
                message: Message::Show,
                payload: vec![],
            })
            .map_err(|_| anyhow::anyhow!("Display command queue busy"))?;
        Ok(())
    }
    pub async fn reset(&mut self, width: u32, height: u32) -> Result<()> {
        let epoch = self.epoch.advance();
        if let Ok(mut s) = self.target.lock() {
            s.width = width;
            s.height = height;
            s.cleanup_complete = false;
        }
        self.send(Input {
            epoch,
            message: Message::Reset {
                width,
                height,
                instrumented: false,
            },
            payload: vec![],
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
        let epoch = self.epoch.current().context("Obsolete video producer")?;
        self.send(Input {
            epoch,
            message: Message::Packet {
                keyframe,
                sequence,
                timestamp_us,
                trace: None,
            },
            payload: bytes,
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
        let epoch = self.epoch.current().context("Obsolete video producer")?;
        if let Some(t) = &trace {
            if let Ok(mut active) = self.telemetry.lock() {
                *active = Some(t.telemetry.clone());
            }
        }
        self.send(Input {
            epoch,
            message: Message::Packet {
                keyframe,
                sequence,
                timestamp_us,
                trace: trace.map(|t| TraceSeed {
                    capture_us: t.capture_us,
                    offset_us: t.offset_us,
                    uncertainty_us: t.uncertainty_us,
                    queued_host_us: now_us() as i64 + t.offset_us,
                }),
            },
            payload: bytes,
        })
        .await
    }
    async fn send(&self, mut input: Input) -> Result<()> {
        if let Some(message) = &self.unavailable {
            bail!("{message}");
        }
        if input.payload.len() > bridge_core::protocol::MAX_MEDIA {
            bail!("Video packet is too large");
        }
        let start = Instant::now();
        loop {
            if self.owner.stop.load(Ordering::Acquire) {
                bail!("Native video consumer stopped");
            }
            match self.input.try_send(input) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(p)) => input = p,
                Err(_) => bail!("Native worker supervisor stopped"),
            }
            if start.elapsed() > Duration::from_secs(5) {
                bail!("Native worker queue stalled");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}
fn fail(target: &Arc<Mutex<VideoFrame>>, diagnostics: &Diagnostics, message: String) {
    diagnostics.record("native-worker", &message);
    if let Ok(mut s) = target.lock() {
        if s.error.is_none() {
            s.error = Some(message);
        }
        s.closed = true;
    }
}
struct Process {
    child: Child,
    socket: UnixStream,
    reader: Option<thread::JoinHandle<()>>,
    stopped: mpsc::Receiver<()>,
    path: std::path::PathBuf,
    clock: ClockEstimate,
}
impl Process {
    fn finish(&mut self) -> Result<()> {
        let _ = worker::write(&mut self.socket, &Message::Stop, &[]);
        let deadline = Instant::now()
            + if cfg!(test) {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(6)
            };
        let mut normal = false;
        loop {
            if let Some(status) = self.child.try_wait()? {
                normal = status.success();
                break;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if !normal {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let acknowledged = self.stopped.try_recv().is_ok();
        let _ = std::fs::remove_file(&self.path);
        if !normal || !acknowledged {
            bail!("Native worker did not acknowledge clean shutdown (terminated/reaped)");
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}
fn launch(
    target: Arc<Mutex<VideoFrame>>,
    telemetry: Arc<Mutex<Option<Arc<Telemetry>>>>,
    diagnostics: Arc<Diagnostics>,
    stop: Arc<AtomicBool>,
) -> Result<Process> {
    let executable = std::env::current_exe()?;
    launch_using(target, telemetry, diagnostics, stop, |path| {
        let mut command = Command::new(executable);
        command.arg("--native-video-worker").arg(path);
        command
    })
}
struct SpawnLease(Option<Child>);
impl Drop for SpawnLease {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
struct SocketPath(std::path::PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn launch_using(
    target: Arc<Mutex<VideoFrame>>,
    telemetry: Arc<Mutex<Option<Arc<Telemetry>>>>,
    diagnostics: Arc<Diagnostics>,
    stop: Arc<AtomicBool>,
    command: impl FnOnce(&std::path::Path) -> Command,
) -> Result<Process> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut random = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let secret = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = std::path::Path::new("/tmp").join(format!(
        "airbridge-{}-{}-{secret}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = UnixListener::bind(&path)?;
    let path_guard = SocketPath(path.clone());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let mut child_owner = SpawnLease(Some(command(&path).spawn()?));
    let child = child_owner
        .0
        .as_mut()
        .context("Native process ownership missing")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&path);
                return Err(error.into());
            }
        }
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline || child.try_wait()?.is_some()
        {
            let _ = child.kill();
            let status = child.wait();
            let _ = std::fs::remove_file(&path);
            bail!("Native worker startup failed: {status:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(listener);
    let _ = std::fs::remove_file(&path);
    socket.set_nonblocking(false)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let t1 = now_us();
    worker::write(&mut socket, &Message::ClockProbe { t1 }, &[])?;
    let clock = loop {
        let (message, _) = worker::read(&mut socket)?;
        if let Message::ClockReply { t1: echo, t2, t3 } = message {
            if echo != t1 {
                bail!("Invalid worker clock reply");
            }
            break ClockEstimate::from_probe(t1, t2, t3, now_us())
                .context("Invalid worker clock estimate")?;
        }
    };
    socket.set_read_timeout(None)?;
    let mut events = socket.try_clone()?;
    let (stopped_tx, stopped) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("airbridge-native-status".into())
        .spawn(move || {
            while let Ok((message, _)) = worker::read(&mut events) {
                match message {
                    Message::Status(status) => {
                        if let Ok(mut s) = target.lock() {
                            s.width = status.width;
                            s.height = status.height;
                            s.frames = status.frames;
                            s.presented = status.presented;
                            s.gpu_memory_bytes = status.gpu_memory_bytes;
                            s.gpu_in_flight = status.gpu_in_flight;
                            s.frame_age_us = status.frame_age_us;
                            s.skipped = status.skipped;
                            s.decoder = status.decoder;
                            s.user_closed = status.user_closed;
                            if s.error.is_none() {
                                s.error = status.error;
                            }
                            s.closed |= status.closed;
                        }
                    }
                    Message::Error(message) => fail(&target, &diagnostics, message),
                    Message::Log(line) => diagnostics.record("native-worker", line),
                    Message::Diagnostics { events, lost } => {
                        if let Some(t) = telemetry.lock().ok().and_then(|t| t.clone()) {
                            t.peer_lost(lost);
                            for event in events {
                                t.trace(event);
                            }
                        }
                    }
                    Message::Stopped => {
                        let _ = stopped_tx.send(());
                        break;
                    }
                    _ => {
                        fail(
                            &target,
                            &diagnostics,
                            "Invalid native worker response".into(),
                        );
                        break;
                    }
                }
            }
        })?;
    drop(path_guard);
    Ok(Process {
        child: child_owner
            .0
            .take()
            .context("Native child ownership missing")?,
        socket,
        reader: Some(reader),
        stopped,
        path,
        clock,
    })
}
fn supervise<const DEBUG: bool>(
    rx: mpsc::Receiver<Input>,
    stop: Arc<AtomicBool>,
    target: Arc<Mutex<VideoFrame>>,
    telemetry: Arc<Mutex<Option<Arc<Telemetry>>>>,
    diagnostics: Arc<Diagnostics>,
) {
    let mut process: Option<Process> = None;
    let mut config = (1280, 720);
    let mut generation = 0;
    while !stop.load(Ordering::Acquire) {
        if let Some(p) = process.as_mut() {
            match p.child.try_wait() {
                Ok(Some(status)) => {
                    fail(
                        &target,
                        &diagnostics,
                        format!("Native video worker exited: {status}"),
                    );
                    break;
                }
                Err(error) => {
                    fail(&target, &diagnostics, error.to_string());
                    break;
                }
                _ => {}
            }
        }
        let mut input = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        match input.message {
            Message::Reset { width, height, .. } => {
                if input.epoch <= generation {
                    continue;
                }
                generation = input.epoch;
                config = (width, height);
                if let Some(p) = process.as_mut() {
                    if let Err(error) = worker::write(
                        &mut p.socket,
                        &Message::Reset {
                            width,
                            height,
                            instrumented: DEBUG,
                        },
                        &[],
                    ) {
                        fail(&target, &diagnostics, error.to_string());
                        break;
                    }
                }
            }
            Message::Packet { keyframe, .. } => {
                if input.epoch != generation {
                    continue;
                }
                if process.is_none() {
                    if !keyframe {
                        continue;
                    }
                    match launch(
                        target.clone(),
                        telemetry.clone(),
                        diagnostics.clone(),
                        stop.clone(),
                    ) {
                        Ok(mut p) => {
                            if let Err(error) = worker::write(
                                &mut p.socket,
                                &Message::Reset {
                                    width: config.0,
                                    height: config.1,
                                    instrumented: DEBUG,
                                },
                                &[],
                            ) {
                                fail(&target, &diagnostics, error.to_string());
                                break;
                            }
                            process = Some(p);
                        }
                        Err(error) => {
                            fail(&target, &diagnostics, format!("{error:#}"));
                            break;
                        }
                    }
                }
                if let Some(p) = process.as_mut() {
                    if DEBUG {
                        if let Message::Packet {
                            trace: Some(seed), ..
                        } = &mut input.message
                        {
                            seed.offset_us = seed.offset_us.saturating_sub(p.clock.offset_us);
                            seed.uncertainty_us = seed
                                .uncertainty_us
                                .map(|u| u.saturating_add(p.clock.uncertainty_us));
                        }
                    }
                    if let Err(error) = worker::write(&mut p.socket, &input.message, &input.payload)
                    {
                        fail(
                            &target,
                            &diagnostics,
                            format!("Native media IPC failed: {error}"),
                        );
                        break;
                    }
                }
            }
            Message::Show => {
                if let Some(p) = process.as_mut() {
                    let _ = worker::write(&mut p.socket, &Message::Show, &[]);
                }
            }
            _ => {}
        }
    }
    if let Some(mut p) = process {
        if let Err(error) = p.finish() {
            fail(&target, &diagnostics, error.to_string());
        }
    }
    if let Ok(mut s) = target.lock() {
        s.closed = true;
        s.cleanup_complete = true;
    }
    diagnostics.record(
        "native-worker",
        "Video consumer stopped and child process reaped",
    );
}
/// Child entry uses the same executable. GPUI and host networking never run here.
pub fn worker_entry() -> bool {
    let mut args = std::env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--native-video-worker")) {
        return false;
    }
    let result = args
        .next()
        .context("Missing worker socket")
        .and_then(|path| run_child(path));
    if let Err(error) = result {
        eprintln!("[native-worker] {error:#}");
        std::process::exit(1);
    }
    true
}
fn run_child(path: std::ffi::OsString) -> Result<()> {
    let mut input = UnixStream::connect(path)?;
    input.set_write_timeout(Some(Duration::from_secs(2)))?;
    let output = Arc::new(Mutex::new(input.try_clone()?));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let diagnostics = Arc::new(Diagnostics::new("viewer-native"));
    let frame = Arc::new(Mutex::new(crate::native_video::VideoFrame::default()));
    let telemetry = Telemetry::new();
    let done = Arc::new(AtomicBool::new(false));
    let tick_done = done.clone();
    let tick_output = output.clone();
    let tick_frame = frame.clone();
    let tick_telemetry = telemetry.clone();
    let tick_logs = diagnostics.clone();
    let events = thread::spawn(move || {
        let mut log_cursor = 0;
        while !tick_done.load(Ordering::Acquire) {
            let status = tick_frame
                .lock()
                .map(|s| worker::Status {
                    width: s.width,
                    height: s.height,
                    frames: s.frames,
                    presented: s.presented,
                    gpu_memory_bytes: s.gpu_memory_bytes,
                    gpu_in_flight: s.gpu_in_flight,
                    frame_age_us: s.frame_age_us,
                    skipped: s.skipped,
                    decoder: s.decoder.clone(),
                    error: s.error.clone(),
                    closed: s.closed,
                    user_closed: s.user_closed,
                })
                .unwrap_or_default();
            if let Ok(mut socket) = tick_output.lock() {
                if worker::write(&mut *socket, &Message::Status(status), &[]).is_err() {
                    break;
                }
                for (id, line) in tick_logs.since(log_cursor, 16) {
                    worker::write(
                        &mut *socket,
                        &Message::Log(line.chars().take(512).collect()),
                        &[],
                    )
                    .ok();
                    log_cursor = id;
                }
                let batch = tick_telemetry.drain(0, 0);
                for chunk in batch.events.chunks(24) {
                    if worker::write(
                        &mut *socket,
                        &Message::Diagnostics {
                            events: chunk.to_vec(),
                            lost: batch.lost_events,
                        },
                        &[],
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    let mut window: Option<crate::native_video::VideoWindow> = None;
    let mut config = (1280, 720, false);
    let mut submit: fn(
        &tokio::runtime::Runtime,
        &crate::native_video::VideoWindow,
        Vec<u8>,
        bool,
        u64,
        u64,
        Option<TraceSeed>,
        Arc<Telemetry>,
    ) -> Result<()> = child_submit::<false>;
    let result: Result<()> = (|| {
        loop {
            let (message, payload) = worker::read(&mut input)?;
            match message {
                Message::Reset {
                    width,
                    height,
                    instrumented,
                } => {
                    if width == 0 || height == 0 || width > 7680 || height > 4320 {
                        bail!("Invalid worker video dimensions");
                    }
                    config = (width, height, instrumented);
                    submit = if instrumented {
                        child_submit::<true>
                    } else {
                        child_submit::<false>
                    };
                    if let Some(window) = window.as_mut() {
                        runtime.block_on(window.reset(width, height))?;
                    }
                }
                Message::Packet {
                    keyframe,
                    sequence,
                    timestamp_us,
                    trace,
                } => {
                    if window.is_none() {
                        if !keyframe {
                            continue;
                        }
                        let mut created = crate::native_video::VideoWindow::spawn_mode(
                            frame.clone(),
                            diagnostics.clone(),
                            config.2,
                        )?;
                        runtime.block_on(created.reset(config.0, config.1))?;
                        window = Some(created);
                    }
                    let window = window.as_ref().context("Native window unavailable")?;
                    submit(
                        &runtime,
                        window,
                        payload,
                        keyframe,
                        sequence,
                        timestamp_us,
                        trace,
                        telemetry.clone(),
                    )?;
                }
                Message::ClockProbe { t1 } => {
                    let t2 = now_us();
                    if let Ok(mut socket) = output.lock() {
                        worker::write(
                            &mut *socket,
                            &Message::ClockReply {
                                t1,
                                t2,
                                t3: now_us(),
                            },
                            &[],
                        )?;
                    }
                }
                Message::Show => {
                    if let Some(window) = &window {
                        window.show()?;
                    }
                }
                Message::Stop => break,
                _ => bail!("Invalid parent worker command"),
            }
        }
        Ok(())
    })();
    done.store(true, Ordering::Release);
    let _ = events.join();
    if let Some(window) = window {
        window.close_wait()?;
        drop(window);
    }
    crate::native_video::VideoWindow::shutdown_backend();
    if let Err(error) = &result {
        if let Ok(mut socket) = output.lock() {
            let _ = worker::write(&mut *socket, &Message::Error(format!("{error:#}")), &[]);
        }
    }
    if let Ok(mut socket) = output.lock() {
        worker::write(&mut *socket, &Message::Stopped, &[])?;
    }
    result
}

fn child_submit<const DEBUG: bool>(
    runtime: &tokio::runtime::Runtime,
    window: &crate::native_video::VideoWindow,
    payload: Vec<u8>,
    keyframe: bool,
    sequence: u64,
    timestamp_us: u64,
    trace: Option<TraceSeed>,
    telemetry: Arc<Telemetry>,
) -> Result<()> {
    if DEBUG {
        let token = trace.map(|t| {
            let end = now_us() as i64 + t.offset_us;
            telemetry.trace(TraceEvent {
                sequence,
                capture_us: t.capture_us,
                uncertainty_us: t.uncertainty_us,
                stage: Stage::IpcQueue,
                sample: Sample {
                    start_us: t.queued_host_us,
                    duration_us: Some(end.saturating_sub(t.queued_host_us).max(0) as u64),
                    outcome: Outcome::Complete,
                    reason: "Compressed packet IPC and native worker scheduling".into(),
                    estimated: true,
                },
            });
            TraceToken {
                sequence,
                capture_us: t.capture_us,
                offset_us: t.offset_us,
                uncertainty_us: t.uncertainty_us,
                telemetry: telemetry.clone(),
            }
        });
        runtime.block_on(window.submit_debug(payload, keyframe, sequence, timestamp_us, token))
    } else {
        runtime.block_on(window.submit(payload, keyframe, sequence, timestamp_us))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Invoked in a separate test process. No native window/GPU APIs are called.
    #[test]
    fn fixture() {
        let Some(path) = std::env::var_os("AIRBRIDGE_FIXTURE_SOCKET") else {
            return;
        };
        let mode = std::env::var("AIRBRIDGE_FIXTURE_MODE").unwrap_or_default();
        if mode == "startup-failure" {
            std::process::exit(7);
        }
        let mut socket = UnixStream::connect(path).unwrap();
        loop {
            let Ok((message, _)) = worker::read(&mut socket) else {
                break;
            };
            match message {
                Message::ClockProbe { t1 } => {
                    let t2 = now_us();
                    worker::write(
                        &mut socket,
                        &Message::ClockReply {
                            t1,
                            t2,
                            t3: now_us(),
                        },
                        &[],
                    )
                    .unwrap();
                }
                Message::Packet { .. } if mode == "crash" => std::process::exit(139),
                Message::Stop if mode == "hang" => loop {
                    thread::sleep(Duration::from_millis(50));
                },
                Message::Stop => {
                    worker::write(&mut socket, &Message::Stopped, &[]).unwrap();
                    break;
                }
                _ => {}
            }
        }
    }
    fn fixture_process(mode: &str) -> Result<Process> {
        launch_using(
            Arc::new(Mutex::new(VideoFrame::default())),
            Arc::new(Mutex::new(None)),
            Arc::new(Diagnostics::new("worker-test")),
            Arc::new(AtomicBool::new(false)),
            |path| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .arg("--exact")
                    .arg("linux::tests::fixture")
                    .arg("--nocapture")
                    .env("AIRBRIDGE_FIXTURE_SOCKET", path)
                    .env("AIRBRIDGE_FIXTURE_MODE", mode);
                command
            },
        )
    }
    #[test]
    fn clean_worker_acknowledges_and_is_reaped() {
        let mut p = fixture_process("normal").unwrap();
        assert!(p.finish().is_ok());
        assert!(p.child.try_wait().unwrap().is_some());
    }
    #[test]
    fn crashed_worker_is_contained_and_reaped() {
        let mut p = fixture_process("crash").unwrap();
        worker::write(
            &mut p.socket,
            &Message::Packet {
                keyframe: true,
                sequence: 1,
                timestamp_us: 1234,
                trace: None,
            },
            b"compressed",
        )
        .unwrap();
        assert!(p.finish().is_err());
        assert!(p.child.try_wait().unwrap().is_some());
    }
    #[test]
    fn hung_worker_is_terminated_without_freeing_parent_native_resources() {
        let mut p = fixture_process("hang").unwrap();
        let start = Instant::now();
        assert!(p.finish().is_err());
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(p.child.try_wait().unwrap().is_some());
    }
    #[test]
    fn failed_worker_startup_returns_an_error() {
        assert!(fixture_process("startup-failure").is_err());
    }
    #[test]
    fn host_error_before_keyframe_starts_no_native_process() {
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        let window =
            VideoWindow::spawn(frame.clone(), Arc::new(Diagnostics::new("no-native-start")))
                .unwrap();
        window.close().unwrap();
        window.owner.join();
        assert!(frame.lock().unwrap().cleanup_complete);
    }
}
