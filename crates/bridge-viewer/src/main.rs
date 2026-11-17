#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod linux;
#[cfg(target_os = "linux")]
mod native_video;
mod presentation;
#[cfg(any(target_os = "linux", test))]
mod resource_lifecycle;
#[cfg(target_os = "linux")]
mod stream_engine;

mod app {
    use super::linux::{VideoFrame, VideoWindow};
    use anyhow::{Result, bail};
    use bridge_core::sessions::{
        DecoderChoice, DisplayTarget, Scaling, UpscaleFilter, ViewerDisplay, WindowMode,
    };
    use bridge_core::{
        config::{Acceleration, Codec, CodecOption},
        diagnostics::Diagnostics,
        discovery::{DiscoveredHost, DiscoveryEvent, browse},
        protocol::{
            ClientHello, ClientMessage, HostMessage, MAX_SCREENS, MediaKind, MediaLane, MediaState,
            MediaStreamHello, RequestId, ScreenConfig, ScreenId, StreamGeneration, VERSION,
            control_reader, receive_debug_media_or_end, receive_json, receive_media_or_end,
            send_control,
        },
        security::{authenticate_viewer_screen, client_endpoint},
        telemetry::{
            ClockEstimate, Outcome, Stage, StatsView, StreamStats, Telemetry, TimingView,
            TraceToken, now_us,
        },
    };
    use bridge_ui::{
        fields::{Fields, InputSpec},
        kit::{self, Choice, Pill},
    };
    use gpui_kit::component::{
        ActiveTheme, Disableable, IconName, Sizable, TitleBar,
        button::{Button, ButtonVariants},
        input::Input,
        slider::Slider,
    };
    use gpui_kit::{
        AnyElement, AnyWindowHandle, App, Bounds, Context, FontWeight, SharedString, Window,
        WindowBounds, WindowDecorations, WindowOptions, bounds, div, point, prelude::*, px, size,
    };
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{mpsc, watch};

    enum Command {
        /// Address, password or PIN, and whether the viewer dialed on its own.
        Connect(SocketAddr, String, bool),
        Disconnect,
        /// Pause a session screen on this device, or leave a PIN screen.
        RemoveScreen(ScreenId),
        /// Resume a session screen paused on this device.
        AttachScreen(ScreenId),
        RetryScreen(ScreenId),
        /// Re-apply this device's presentation settings to the screen's window.
        Present(ScreenId),
        Diagnostics(ScreenId, bool),
        Burst(ScreenId),
    }

    enum LaneNotice {
        Opened(ScreenId, StreamGeneration, MediaLane),
        FirstFrame(ScreenId, StreamGeneration),
        Failed(ScreenId, StreamGeneration, MediaLane, String),
        Closed(ScreenId, StreamGeneration, MediaLane),
    }

    type LaneKey = (ScreenId, StreamGeneration, MediaLane);
    struct LaneRegistration {
        key: LaneKey,
        active: Arc<Mutex<HashSet<LaneKey>>>,
        events: mpsc::Sender<LaneNotice>,
        shared: Option<Arc<Mutex<State>>>,
    }
    impl Drop for LaneRegistration {
        fn drop(&mut self) {
            if let Some(shared) = &self.shared {
                end_client_lane(shared, self.key.0, self.key.1, "Video lane closed".into());
            }
            if let Ok(mut active) = self.active.lock() {
                active.remove(&self.key);
            }
            if std::thread::panicking() {
                let (id, generation, lane) = self.key;
                let _ = self.events.try_send(LaneNotice::Failed(
                    id,
                    generation,
                    lane,
                    "Media worker panicked".into(),
                ));
            }
        }
    }

    #[derive(Clone)]
    struct ClientScreen {
        config: ScreenConfig,
        status: String,
        terminal: bool,
        cleanup_reported: bool,
        removing: bool,
        cancel: watch::Sender<bool>,
        frame: Arc<Mutex<VideoFrame>>,
        video_window: VideoWindow,
        diagnostics: Arc<Diagnostics>,
        bytes: u64,
        received_frames: u64,
        host_frames_sent: u64,
        host_frames_dropped: u64,
        bitrate_kbps: u32,
        telemetry: Arc<Telemetry>,
        stats_view: StatsView,
    }

    fn ingest_diagnostic_batch(
        shared: &Arc<Mutex<State>>,
        batch: bridge_core::telemetry::DiagnosticBatch,
    ) {
        if let Ok(state) = shared.lock() {
            if let Some(screen) = state.screens.iter().find(|s| {
                s.config.screen_id == batch.screen_id
                    && s.config.generation == batch.generation
                    && s.config.instrumented
            }) {
                screen.telemetry.peer_lost(batch.lost_events);
                if let Some(stats) = batch.stats {
                    screen.telemetry.peer_stats(stats);
                }
                for e in batch.events {
                    screen.telemetry.ingest(e);
                }
            }
        }
    }

    #[derive(Clone)]
    struct State {
        clock: ClockEstimate,
        hosts: Vec<DiscoveredHost>,
        /// The temporary screen a PIN connection asks for.
        target_screen: u32,
        /// Name of the host this entry talks to, as last seen on the network.
        host_label: String,
        host_identity: String,
        viewer_identity: u64,
        pin: String,
        username: String,
        stable_viewer: String,
        remembered: Option<bridge_core::sessions::SavedViewerSession>,
        authenticated_key: Option<bridge_core::sessions::Secret>,
        contract: Option<bridge_core::sessions::SessionContract>,
        preferences: bridge_core::sessions::ViewerPreferences,
        remember: bool,
        auto_connect: bool,
        auto_suspended: bool,
        power_notice: String,
        screens: Vec<ClientScreen>,
        removed_screens: HashSet<ScreenId>,
        next_request_id: u64,
        connected: bool,
        connecting: bool,
        message: String,
        terminal_reason: Option<String>,
        retry_requested: bool,
        last_endpoint: Option<SocketAddr>,
        diagnostics: Arc<Diagnostics>,
        bytes: u64,
        received_frames: u64,
    }

    impl State {
        fn viewer_lock_policy(&self) -> bridge_core::sessions::ViewerLockPolicy {
            self.contract
                .as_ref()
                .map(|contract| contract.viewer_lock_policy)
                .unwrap_or_default()
        }
        fn effective_keep_awake(&self) -> bool {
            self.viewer_lock_policy()
                .effective_keep_awake(self.preferences.keep_awake)
        }
        /// The saved session's host, if it is on the network right now.
        fn known_host(&self) -> Option<&DiscoveredHost> {
            let host_id = &self.contract.as_ref()?.host_id;
            self.hosts.iter().find(|host| host.host_id == *host_id)
        }
        /// Where to dial on our own: both sides must have auto-connect on and
        /// the host must currently offer this session.
        fn auto_endpoint(&self) -> Option<SocketAddr> {
            if !self.auto_connect || self.auto_suspended || !self.remember {
                return None;
            }
            let saved = self.remembered.as_ref()?;
            self.hosts
                .iter()
                .filter(|host| {
                    host.offers(&saved.contract.host_id, &saved.contract.session_id)
                        .is_some_and(|session| session.auto_connect)
                })
                .flat_map(|host| host.addresses.iter())
                .copied()
                .next()
        }

        /// Each record replaces the host's previous one wholesale, so screens
        /// and sessions the host no longer lists disappear here too.
        fn discovery_event(&mut self, event: DiscoveryEvent) {
            match event {
                DiscoveryEvent::Found(host) => {
                    if self
                        .contract
                        .as_ref()
                        .is_some_and(|c| c.host_id == host.host_id)
                    {
                        self.host_label = host.label.clone();
                    }
                    if let Some(existing) =
                        self.hosts.iter_mut().find(|entry| entry.name == host.name)
                    {
                        *existing = host;
                    } else {
                        self.diagnostics
                            .record("discovery", format!("Found {}", host.label));
                        self.hosts.push(host);
                    }
                }
                DiscoveryEvent::Lost(name) => {
                    if let Some(index) = self.hosts.iter().position(|host| host.name == name) {
                        let host = self.hosts.remove(index);
                        self.diagnostics
                            .record("discovery", format!("Host disappeared: {}", host.label));
                    }
                }
            }
        }

        fn screen(&self, id: ScreenId) -> Option<&ClientScreen> {
            self.screens.iter().find(|s| s.config.screen_id == id)
        }
        /// How this device presents a slot; defaults until the user changes it.
        fn display(&self, id: ScreenId) -> ViewerDisplay {
            self.preferences
                .screen(id)
                .map(|screen| screen.display.clone())
                .unwrap_or_default()
        }
    }

    fn message(state: &Arc<Mutex<State>>, text: impl Into<String>) {
        let text = text.into();
        if let Ok(mut state) = state.lock() {
            state.diagnostics.record("viewer", &text);
            state.message = text;
        }
    }

    #[derive(Clone)]
    struct LocalScreen {
        id: u64,
        state: Arc<Mutex<State>>,
        commands: mpsc::Sender<Command>,
        shutdown: watch::Sender<bool>,
        thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    }
    #[derive(Default)]
    struct Workspace {
        entries: Vec<LocalScreen>,
        retiring: Vec<LocalScreen>,
        next_id: u64,
    }
    /// Create a connection entry with its own network thread; `None` once the
    /// entry limit is reached.
    fn add_local_session(
        workspace: &Arc<Mutex<Workspace>>,
        catalog: &Arc<Mutex<State>>,
        saved: Option<bridge_core::sessions::SavedViewerSession>,
    ) -> Option<LocalScreen> {
        let snapshot = catalog.lock().map(|s| s.clone()).unwrap();
        let mut ws = workspace.lock().unwrap();
        ws.retiring.retain(|entry| {
            let finished = entry
                .thread
                .lock()
                .map(|t| t.as_ref().is_none_or(|t| t.is_finished()))
                .unwrap_or(true);
            if finished {
                if let Some(t) = entry.thread.lock().ok().and_then(|mut t| t.take()) {
                    let _ = t.join();
                }
            }
            !finished
        });
        if ws.entries.len() + ws.retiring.len() >= MAX_SCREENS {
            return None;
        }
        ws.next_id += 1;
        let id = ws.next_id;
        let mut state = initial_state();
        state.hosts = snapshot.hosts;
        state.stable_viewer = snapshot.stable_viewer;
        state.message = "Not connected".into();
        if let Some(saved) = saved {
            state.username = saved.contract.username.clone();
            state.contract = Some(saved.contract.clone());
            state.preferences = saved.preferences.clone();
            state.auto_connect = saved.auto_connect;
            state.remember = true;
            state.remembered = Some(saved);
        }

        state.diagnostics = Arc::new(Diagnostics::new(&format!("viewer-local-{id}")));
        let state = Arc::new(Mutex::new(state));
        let (commands, rx) = mpsc::channel(64);
        let (shutdown, stop) = watch::channel(false);
        let network = state.clone();
        let task = std::thread::spawn(move || {
            if let Err(e) = run_network(network.clone(), rx, stop) {
                message(&network, format!("Screen connection failed: {e:#}"));
            }
        });
        let entry = LocalScreen {
            id,
            state,
            commands,
            shutdown,
            thread: Arc::new(Mutex::new(Some(task))),
        };
        ws.entries.push(entry.clone());
        Some(entry)
    }
    fn remove_local_screen(workspace: &Arc<Mutex<Workspace>>, id: u64) {
        let entry = workspace.lock().ok().and_then(|mut ws| {
            let index = ws.entries.iter().position(|e| e.id == id)?;
            Some(ws.entries.remove(index))
        });
        if let Some(entry) = entry {
            if let Ok(mut s) = entry.state.lock() {
                s.auto_suspended = true;
                s.remember = false;
                s.auto_connect = false;
                if let Some(contract) = &s.contract {
                    if let Err(e) = bridge_core::sessions::forget_viewer(&contract.session_id) {
                        s.diagnostics
                            .record("session", format!("Cannot remove remembered session: {e}"));
                    }
                }
            }
            entry.shutdown.send_replace(true);
            let _ = entry.commands.try_send(Command::Disconnect);
            if let Ok(mut ws) = workspace.lock() {
                ws.retiring.push(entry);
            }
        }
    }
    include!("ui.rs");

    fn run_network(
        shared: Arc<Mutex<State>>,
        mut commands: mpsc::Receiver<Command>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let mut reconnect=None;
            let mut auto_timer=tokio::time::interval(Duration::from_secs(1));let mut attempt=0;let mut next_auto=std::time::Instant::now();
            loop {
                let command = if let Some(command)=reconnect.take(){command}else{tokio::select! {
                    _ = shutdown.changed() => break,
                    command = commands.recv() => { let Some(command) = command else { break; }; command }
                    _=auto_timer.tick()=>{
                        if std::time::Instant::now()<next_auto{continue;}
                        let address=shared.lock().ok().and_then(|s|s.auto_endpoint());
                        let Some(address)=address else{continue;};
                        attempt+=1;next_auto=std::time::Instant::now()+bridge_core::sessions::reconnect_delay(attempt);Command::Connect(address,String::new(),true)
                    }
                }};
                if let Command::Connect(address, pin, automatic) = command {
                    if let Ok(mut state) = shared.lock() { state.connecting = true; state.terminal_reason=None; }
                    let began=std::time::Instant::now();
                    let result = connect(address, pin, automatic, shared.clone(), &mut commands, shutdown.clone()).await;
                    if began.elapsed()>Duration::from_secs(30){attempt=0;}
                    next_auto=std::time::Instant::now()+bridge_core::sessions::reconnect_delay(attempt);
                    clear_client_session(&shared);
                    if let Err(error) = result {let cause=shared.lock().ok().and_then(|s|s.terminal_reason.clone());message(&shared,cause.unwrap_or_else(||format!("Connection failed: {error:#}")));}
                    if *shutdown.borrow() { break; }
                    reconnect=shared.lock().ok().and_then(|mut state|{if !state.retry_requested{return None;}state.retry_requested=false;Some(Command::Connect(state.last_endpoint?,state.pin.clone(),false))});
                }
            }
            clear_client_session(&shared);
            Ok(())
        })
    }

    fn clear_client_session(shared: &Arc<Mutex<State>>) {
        let screens = shared
            .lock()
            .map(|mut state| {
                state.removed_screens.clear();
                state.connected = false;
                state.connecting = false;
                state.bytes = 0;
                state.received_frames = 0;
                std::mem::take(&mut state.screens)
            })
            .unwrap_or_default();
        for screen in screens {
            screen.cancel.send_replace(true);
            let _ = screen.video_window.close();
        }
    }

    async fn new_client_screen(config: ScreenConfig, shared: &Arc<Mutex<State>>) -> ClientScreen {
        let screen_id = config.screen_id;
        let identity = shared
            .lock()
            .map(|s| format!("{}-{}", s.host_identity, s.viewer_identity))
            .unwrap_or_default();
        let diagnostics = Arc::new(Diagnostics::new(&format!(
            "viewer-{identity}-screen-{screen_id}"
        )));
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        let telemetry = Telemetry::new();
        let instrumented = config.instrumented;
        let window_frame = frame.clone();
        let window_diagnostics = diagnostics.clone();
        let spawned = tokio::task::spawn_blocking(move || {
            VideoWindow::spawn_mode(window_frame, window_diagnostics, instrumented)
        })
        .await;
        let video_window = match spawned {
            Ok(Ok(window)) => window,
            Ok(Err(error)) => {
                let detail = format!("Display backend unavailable: {error:#}");
                diagnostics.record("video-window", &detail);
                message(shared, detail.clone());
                VideoWindow::unavailable(detail, frame.clone())
            }
            Err(error) => {
                let detail = format!("Display backend task failed: {error}");
                diagnostics.record("video-window", &detail);
                message(shared, detail.clone());
                VideoWindow::unavailable(detail, frame.clone())
            }
        };
        ClientScreen {
            config,
            status: "Waiting for video".into(),
            terminal: false,
            cleanup_reported: false,
            removing: false,
            cancel: watch::channel(false).0,
            frame,
            video_window,
            diagnostics,
            bytes: 0,
            received_frames: 0,
            host_frames_sent: 0,
            host_frames_dropped: 0,
            bitrate_kbps: 0,
            telemetry,
            stats_view: StatsView::default(),
        }
    }

    async fn install_client_screen(shared: &Arc<Mutex<State>>, config: ScreenConfig) -> Result<()> {
        config.settings.validate()?;
        if shared
            .lock()
            .map(|state| state.removed_screens.contains(&config.screen_id))
            .unwrap_or(true)
        {
            return Ok(());
        }
        let existing = shared.lock().ok().and_then(|state| {
            state
                .screens
                .iter()
                .find(|screen| screen.config.screen_id == config.screen_id)
                .cloned()
        });
        if existing.as_ref().is_some_and(|screen| {
            screen.removing
                || screen
                    .frame
                    .lock()
                    .map(|frame| frame.user_closed)
                    .unwrap_or(true)
                || screen.config.generation >= config.generation
        }) {
            return Ok(());
        }
        let mut screen = match existing {
            Some(screen) => {
                screen.cancel.send_replace(true);
                screen.video_window.close_wait().await?;
                let mut replacement = new_client_screen(config.clone(), shared).await;
                replacement.stats_view = screen.stats_view;
                replacement
            }
            None => new_client_screen(config.clone(), shared).await,
        };
        screen.cancel.send_replace(true);
        screen.cancel = watch::channel(false).0;
        screen.terminal = false;
        screen.cleanup_reported = false;
        screen.telemetry = Telemetry::new();
        screen.stats_view.reset();
        screen.config = config.clone();
        screen.status = "Rebuilding decoder and waiting for a keyframe…".into();
        screen.bytes = 0;
        screen.received_frames = 0;
        screen.host_frames_sent = 0;
        screen.host_frames_dropped = 0;
        screen.bitrate_kbps = 0;
        screen.diagnostics.record(
            "config",
            format!(
                "Applied revision {} / generation {} · {}×{} @ {} fps · {:?} {:?}",
                config.revision,
                config.generation,
                config.settings.width,
                config.settings.height,
                config.settings.fps,
                config.codec.codec,
                config.codec.acceleration,
            ),
        );
        let decoder_reset_error = if let Err(error) = screen
            .video_window
            .reset(config.settings.width, config.settings.height)
            .await
        {
            screen
                .diagnostics
                .record("video-window", format!("Decoder reset failed: {error:#}"));
            if let Ok(mut frame) = screen.frame.lock() {
                frame.error = Some(format!("Decoder reset failed: {error:#}"));
                frame.closed = true;
            }
            Some(format!("Decoder reset failed: {error:#}"))
        } else {
            None
        };
        let placement = shared.lock().ok().and_then(|s| {
            s.preferences
                .screens
                .iter()
                .find(|p| p.screen_id == config.screen_id)
                .map(|p| p.window.clone())
        });
        if let Some(placement) = placement {
            if let Err(error) = screen.video_window.restore_window(placement).await {
                screen
                    .diagnostics
                    .record("window", format!("Window restoration unavailable: {error}"));
            }
        }
        // How the picture is shown belongs to this device and is restored per slot.
        let display = shared
            .lock()
            .map(|s| s.display(config.screen_id))
            .unwrap_or_default();
        if let Err(error) = screen.video_window.set_presentation(display).await {
            screen
                .diagnostics
                .record("window", format!("Display settings unavailable: {error}"));
        }
        if let Ok(mut frame) = screen.frame.lock() {
            frame.width = config.settings.width;
            frame.height = config.settings.height;
            frame.frames = 0;
            frame.decoded_sequence = None;
            frame.submitted_sequence = None;
            frame.skipped = 0;
            frame.error = decoder_reset_error;
        }
        if let Ok(mut state) = shared.lock() {
            if let Some(existing) = state
                .screens
                .iter_mut()
                .find(|entry| entry.config.screen_id == config.screen_id)
            {
                *existing = screen.clone();
            } else {
                if state.screens.len() >= MAX_SCREENS {
                    bail!("Host exceeded the {MAX_SCREENS}-screen limit");
                }
                state.screens.push(screen.clone());
            }
        }
        Ok(())
    }

    async fn install_during_session(
        shared: &Arc<Mutex<State>>,
        config: ScreenConfig,
        connection: &quinn::Connection,
        commands: &mut mpsc::Receiver<Command>,
        queued: &mut VecDeque<Command>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<bool> {
        if *shutdown.borrow() {
            return Ok(false);
        }
        let install = install_client_screen(shared, config);
        tokio::pin!(install);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(false),
                reason = connection.closed() => bail!("Host disconnected while opening the display: {reason}"),
                result = &mut install => return result.map(|()| true),
                command = commands.recv() => match command {
                    Some(Command::Disconnect) | None => return Ok(false),
                    Some(command) => {
                        if queued.len()<32 {queued.push_back(command);} else {message(shared,"Too many actions during restart; wait for cleanup and retry the action");}
                    },
                }
            }
        }
    }

    async fn remove_client_screen(shared: &Arc<Mutex<State>>, screen_id: ScreenId) {
        let removed = if let Ok(mut state) = shared.lock() {
            state.removed_screens.insert(screen_id);
            let index = state
                .screens
                .iter()
                .position(|screen| screen.config.screen_id == screen_id);
            let removed = index.map(|index| state.screens.remove(index));
            if let Some(screen) = &removed {
                state.message = format!("{} is no longer shown here", screen.config.name);
            }
            removed
        } else {
            None
        };
        if let Some(screen) = removed {
            screen.cancel.send_replace(true);
            let _ = screen.video_window.close();
        }
    }

    async fn request_remove_screen<W: tokio::io::AsyncWrite + Unpin>(
        shared: &Arc<Mutex<State>>,
        control: &mut W,
        screen_id: ScreenId,
    ) -> Result<()> {
        let screen = shared.lock().ok().and_then(|mut state| {
            let screen = state
                .screens
                .iter_mut()
                .find(|s| s.config.screen_id == screen_id && !s.removing)?;
            screen.removing = true;
            screen.status = "Detaching this viewer…".into();
            screen.diagnostics.record(
                "lifecycle",
                "Closing local display and detaching this viewer; shared host screen is retained",
            );
            let screen = screen.clone();
            state.terminal_reason = Some("Viewer detached · shared host screen retained".into());
            // Within a session this is a pause: remembered, and resumable.
            if state.contract.is_some() {
                if let Some(slot) = state.preferences.screen_mut(screen_id) {
                    slot.visible = false;
                }
            }
            Some(screen)
        });
        let Some(screen) = screen else {
            // Not streaming here (already removing, or the host screen is not
            // live): a session still records the pause on the host.
            let pause = shared
                .lock()
                .map(|mut state| {
                    let pause = state.contract.is_some()
                        && state.screen(screen_id).is_none()
                        && state.removed_screens.insert(screen_id);
                    if pause {
                        if let Some(slot) = state.preferences.screen_mut(screen_id) {
                            slot.visible = false;
                        }
                    }
                    pause
                })
                .unwrap_or(false);
            if pause {
                send_control(
                    control,
                    &ClientMessage::RemoveScreen {
                        request_id: next_request_id(shared),
                        screen_id,
                    },
                )
                .await?;
            }
            return Ok(());
        };
        screen.cancel.send_replace(true);
        let _ = screen.video_window.close();
        if shared.lock().map(|s| s.contract.is_some()).unwrap_or(false) {
            send_control(
                control,
                &ClientMessage::RemoveScreen {
                    request_id: next_request_id(shared),
                    screen_id,
                },
            )
            .await
        } else {
            if let Ok(mut s) = shared.lock() {
                s.auto_suspended = true;
            }
            send_control(control, &ClientMessage::Stop).await
        }
    }

    fn end_client_lane(
        shared: &Arc<Mutex<State>>,
        id: ScreenId,
        generation: StreamGeneration,
        detail: String,
    ) {
        let screen = shared.lock().ok().and_then(|mut state| {
            let screen = state.screens.iter_mut().find(|s| {
                s.config.screen_id == id && s.config.generation == generation && !s.removing
            })?;
            if screen.terminal {
                if screen.status != "Video lane closed" || detail == "Video lane closed" {
                    return None;
                }
            }
            screen.terminal = true;
            screen.status = detail.clone();
            screen.diagnostics.record("lifecycle", &detail);
            let screen = screen.clone();
            if state.terminal_reason.is_none() || detail != "Video lane closed" {
                state.terminal_reason = Some(detail.clone());
            }
            state.message = format!(
                "{}: {detail} · retry or remove this stream",
                screen.config.name
            );
            Some(screen)
        });
        if let Some(screen) = screen {
            let clock = shared.lock().map(|s| s.clock).unwrap_or_default();
            screen
                .telemetry
                .finish(now_us() as i64 + clock.offset_us, &detail);
            if let Ok(mut frame) = screen.frame.lock() {
                frame.error = Some(detail);
            }
            screen.cancel.send_replace(true);
            let _ = screen.video_window.close();
        }
    }

    async fn wait_for_client_screen(
        shared: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        generation: StreamGeneration,
    ) -> Result<ClientScreen> {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(state) = shared.lock() {
                    if let Some(screen) = state.screens.iter().find(|screen| screen.config.screen_id == screen_id) {
                        if screen.config.generation == generation {
                            return Ok(screen.clone());
                        }
                        if screen.config.generation > generation {
                            bail!("Discarded obsolete video generation {generation} for screen {screen_id}");
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("Timed out waiting for screen {screen_id} configuration"))?
    }

    async fn receive_video_lane(
        media_recv: quinn::RecvStream,
        screen: ClientScreen,
        generation: StreamGeneration,
        shared: Arc<Mutex<State>>,
        events: mpsc::Sender<LaneNotice>,
    ) -> Result<()> {
        if screen.config.instrumented {
            receive_video_lane_impl::<true>(media_recv, screen, generation, shared, events).await
        } else {
            receive_video_lane_impl::<false>(media_recv, screen, generation, shared, events).await
        }
    }
    async fn receive_video_lane_impl<const DEBUG: bool>(
        mut media_recv: quinn::RecvStream,
        screen: ClientScreen,
        generation: StreamGeneration,
        shared: Arc<Mutex<State>>,
        events: mpsc::Sender<LaneNotice>,
    ) -> Result<()> {
        let screen_id = screen.config.screen_id;
        let mut first_frame = true;
        let mut cancel = screen.cancel.subscribe();
        loop {
            if *cancel.borrow() {
                let _ = media_recv.stop(0u32.into());
                return Ok(());
            }
            let receive = async {
                if DEBUG {
                    receive_debug_media_or_end(&mut media_recv).await
                } else {
                    Ok(receive_media_or_end(&mut media_recv)
                        .await?
                        .map(|(h, b)| (h, None, 0, b)))
                }
            };
            let packet = tokio::select! {
                _ = cancel.changed() => { let _ = media_recv.stop(0u32.into()); return Ok(()); }
                packet = receive => packet?,
            };
            let Some((header, capture_us, send_start_us, bytes)) = packet else {
                return Ok(());
            };
            if !matches!(&header.kind, MediaKind::Video) {
                bail!("The video lane received a non-video packet");
            }
            if header.screen_id != screen_id || header.generation != generation {
                bail!("Video packet screen/generation does not match its QUIC lane");
            }
            if !(320..=7680).contains(&header.width)
                || !(240..=4320).contains(&header.height)
                || (header.width as u64) * (header.height as u64) > 7680 * 4320
            {
                bail!(
                    "Host sent unsupported video dimensions {}×{}",
                    header.width,
                    header.height
                );
            }
            if let Ok(mut state) = shared.lock() {
                if let Some(entry) = state
                    .screens
                    .iter_mut()
                    .find(|entry| entry.config.screen_id == screen_id)
                {
                    if entry.config.generation != generation || entry.terminal || entry.removing {
                        return Ok(());
                    }
                    entry.bytes = entry.bytes.saturating_add(bytes.len() as u64);
                    entry.received_frames = entry.received_frames.saturating_add(1);
                    state.bytes = state.bytes.saturating_add(bytes.len() as u64);
                    state.received_frames = state.received_frames.saturating_add(1);
                }
            }
            let trace = if DEBUG {
                capture_us.map(|capture_us| {
                    let clock = shared.lock().map(|s| s.clock).unwrap_or_default();
                    let t = TraceToken {
                        sequence: header.sequence,
                        capture_us,
                        offset_us: clock.offset_us,
                        uncertainty_us: Some(if clock.measured_at_us == 0 {
                            u64::MAX
                        } else {
                            clock.uncertainty_us
                                + now_us().saturating_sub(clock.measured_at_us) / 1000
                        }),
                        telemetry: screen.telemetry.clone(),
                    };
                    let arrival=now_us();
                    if clock.measured_at_us!=0 {
                        let end=arrival as i128+clock.offset_us as i128;
                        if end>=send_start_us as i128 {t.mark(Stage::Transfer,send_start_us.saturating_sub_signed(clock.offset_us),Some(arrival),Outcome::Complete,"QUIC write + network + receiver scheduling; estimated clock mapping");}
                        else{t.mark(Stage::Transfer,arrival,None,Outcome::Unavailable,"Clock uncertainty exceeds observed transfer interval");}
                    }else{t.mark(Stage::Transfer,arrival,None,Outcome::Unavailable,"Clock synchronization pending");}
                    t.mark(
                        Stage::Receive,
                        now_us(),
                        None,
                        Outcome::Complete,
                        "Complete packet received, not first network byte",
                    );
                    t.mark(Stage::DecodeQueue, now_us(), None, Outcome::Pending, "");
                    t.mark(
                        Stage::Presentation,
                        now_us(),
                        None,
                        Outcome::Unavailable,
                        "Monitor scanout and partial rendering feedback unavailable",
                    );
                    t
                })
            } else {
                None
            };
            let submit = async {
                if DEBUG {
                    screen
                        .video_window
                        .submit_debug(
                            bytes,
                            header.keyframe,
                            header.sequence,
                            header.timestamp_us,
                            trace.clone(),
                        )
                        .await
                } else {
                    screen
                        .video_window
                        .submit(bytes, header.keyframe, header.sequence, header.timestamp_us)
                        .await
                }
            };
            tokio::select! {
                _ = cancel.changed() => { if DEBUG {if let Some(t)=&trace {t.mark(Stage::DecodeQueue,now_us(),Some(now_us()),Outcome::Cancelled,"Stream replaced or closed");}} let _ = media_recv.stop(0u32.into()); return Ok(()); }
                result = submit => {if DEBUG {if let (Some(t),Err(e))=(&trace,&result){t.mark(Stage::DecodeQueue,now_us(),Some(now_us()),Outcome::Failed,&e.to_string());}} result?},
            }
            if first_frame {
                first_frame = false;
                let _ = events.try_send(LaneNotice::FirstFrame(screen_id, generation));
            }
        }
    }

    async fn receive_lanes(
        connection: quinn::Connection,
        shared: Arc<Mutex<State>>,
        events: mpsc::Sender<LaneNotice>,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        let active = Arc::new(Mutex::new(
            HashSet::<(ScreenId, StreamGeneration, MediaLane)>::new(),
        ));
        loop {
            tokio::select! {
                incoming = connection.accept_uni() => {
                    let Ok(mut stream) = incoming else { break; };
                    if tasks.len()>=8{let _=stream.stop(1u32.into());continue;}
                    let shared = shared.clone();
                    let events = events.clone();
                    let active = active.clone();
                    tasks.spawn(async move {
                        let result = tokio::time::timeout(Duration::from_secs(5), receive_json::<_, MediaStreamHello>(&mut stream)).await;
                        let hello = match result {
                            Ok(Ok(hello)) => hello,
                            Ok(Err(error)) => return (0, 0, MediaLane::Video, Err(error.context("Invalid media stream preamble"))),
                            Err(_) => return (0, 0, MediaLane::Video, Err(anyhow::anyhow!("Media stream preamble timed out"))),
                        };
                        let (screen_id, generation, lane) = (hello.screen_id, hello.generation, hello.lane);
                        if hello.version != VERSION {
                            return (screen_id, generation, lane, Err(anyhow::anyhow!("Incompatible media protocol version {}", hello.version)));
                        }
                        if lane == MediaLane::Diagnostics {
                            let state=shared.clone();
                            let _=bridge_core::telemetry_transport::receive_batches(stream,move |batch| ingest_diagnostic_batch(&state,batch)).await;
                            return (0,0,lane,Ok(()));
                        }
                        if lane != MediaLane::Video {
                            let _ = stream.stop(0u32.into());
                            return (screen_id, generation, lane, Err(anyhow::anyhow!("The {lane:?} lane is not implemented yet")));
                        }
                        if let Err(error) = hello.video.ok_or_else(|| anyhow::anyhow!("Video format contract missing")).and_then(|format| format.validate()) {
                            return (screen_id, generation, lane, Err(error));
                        }
                        let key = (screen_id, generation, lane);
                        if let Ok(mut active) = active.lock() {
                            if !active.insert(key) {
                                let _ = stream.stop(0u32.into());
                                return (0, 0, lane, Err(anyhow::anyhow!("Duplicate video lane for screen {screen_id}, generation {generation}")));
                            }
                        }
                        let _registration = LaneRegistration { key, active: active.clone(), events: events.clone(),shared:Some(shared.clone()) };
                        let outcome = match wait_for_client_screen(&shared, screen_id, generation).await {
                            Ok(screen) => {
                                let _ = events.try_send(LaneNotice::Opened(screen_id, generation, lane));
                                receive_video_lane(stream, screen, generation, shared, events.clone()).await
                            }
                            Err(error) => Err(error),
                        };
                        (screen_id, generation, lane, outcome)
                    });
                }
                finished = tasks.join_next(), if !tasks.is_empty() => {
                    match finished {
                        Some(Ok((screen_id, generation, lane, outcome))) => match outcome {
                            Ok(()) => { if lane!=MediaLane::Diagnostics {let _ = events.try_send(LaneNotice::Closed(screen_id, generation, lane));} },
                            Err(error) => { let _ = events.try_send(LaneNotice::Failed(screen_id, generation, lane, format!("{error:#}"))); },
                        },
                        Some(Err(error)) => {
                            let _ = events.try_send(LaneNotice::Failed(0, 0, MediaLane::Video, format!("Media lane task failed: {error}")));
                        }
                        None => {},
                    }
                }
            }
        }
    }

    fn next_request_id(shared: &Arc<Mutex<State>>) -> RequestId {
        if let Ok(mut state) = shared.lock() {
            let request_id = state.next_request_id;
            state.next_request_id = state.next_request_id.saturating_add(1);
            request_id
        } else {
            0
        }
    }

    async fn connect(
        address: SocketAddr,
        pin: String,
        automatic: bool,
        shared: Arc<Mutex<State>>,
        commands: &mut mpsc::Receiver<Command>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let pin = bridge_core::sessions::Secret(pin);
        let (login, remembered) = shared
            .lock()
            .map(|s| {
                let login = (!s.username.is_empty()).then(|| bridge_core::security::SessionLogin {
                    username: s.username.clone(),
                    session_id: s.contract.as_ref().map(|c| c.session_id.clone()),
                    host_id: s.contract.as_ref().map(|c| c.host_id.clone()).or_else(|| {
                        s.hosts
                            .iter()
                            .find(|h| h.addresses.contains(&address))
                            .map(|h| h.host_id.clone())
                    }),
                    viewer_id: s.stable_viewer.clone(),
                });
                (
                    login,
                    s.remembered
                        .as_ref()
                        .map(|v| v.key.clone())
                        .or_else(|| s.authenticated_key.clone()),
                )
            })
            .map_err(|_| anyhow::anyhow!("Viewer state unavailable"))?;
        if login.is_none() {
            bridge_core::security::validate_pin(&pin)?;
        }
        if let Ok(mut state) = shared.lock() {
            state.last_endpoint = Some(address);
            if let Some(label) = state
                .hosts
                .iter()
                .find(|h| h.addresses.contains(&address))
                .map(|h| h.label.clone())
            {
                state.host_label = label;
            }
        }
        message(&shared, format!("Connecting to {address}…"));
        let setup = async {
            let endpoint = client_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
            let connection = tokio::time::timeout(
                Duration::from_secs(8),
                endpoint.connect(address, "airbridge.local")?,
            )
            .await??;
            let session_auth = if let Some(login) = &login {
                Some(
                    tokio::time::timeout(
                        Duration::from_secs(20),
                        bridge_core::security::authenticate_viewer_session(
                            &connection,
                            login,
                            &pin,
                            remembered.as_ref(),
                        ),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Session login timed out"))??,
                )
            } else {
                tokio::time::timeout(
                    Duration::from_secs(12),
                    authenticate_viewer_screen(
                        &connection,
                        &pin,
                        shared.lock().map(|s| s.target_screen).unwrap_or(1),
                    ),
                )
                .await
                .map_err(|_| anyhow::anyhow!("PIN entry timed out"))??;
                None
            };
            let (mut control_send, mut control_recv) = connection.open_bi().await?;
            let codecs = vec![
                CodecOption {
                    codec: Codec::H264,
                    acceleration: Acceleration::Hardware,
                },
                CodecOption {
                    codec: Codec::H264,
                    acceleration: Acceleration::Software,
                },
            ];
            let hello = ClientHello {
                version: VERSION,
                codecs,
                // The viewer never changes the host's stream; it only presents it.
                initial_settings: None,
                name: bridge_core::config::device_name(),
            };
            let greeting = if let Some(login) = &login {
                let preferences = shared
                    .lock()
                    .map(|s| s.preferences.clone())
                    .unwrap_or_default();
                ClientMessage::SessionHello {
                    hello,
                    viewer_id: login.viewer_id.clone(),
                    preferences,
                    automatic,
                }
            } else {
                ClientMessage::Hello(hello)
            };
            send_control(&mut control_send, &greeting).await?;
            let accepted: HostMessage =
                tokio::time::timeout(Duration::from_secs(15), receive_json(&mut control_recv))
                    .await
                    .map_err(|_| anyhow::anyhow!("Host session acceptance timed out"))??;
            let screens = match accepted {
                HostMessage::PersistentSessionAccepted {
                    contract,
                    screens,
                    viewer_id,
                } => {
                    contract.validate()?;
                    let (session_id, key) = session_auth
                        .ok_or_else(|| anyhow::anyhow!("Unexpected session acceptance"))?;
                    if session_id != contract.session_id
                        || login
                            .as_ref()
                            .is_none_or(|l| l.username != contract.username)
                        || screens.len() > MAX_SCREENS
                        || screens.iter().any(|s| {
                            !contract
                                .screens
                                .iter()
                                .any(|saved| saved.screen_id == s.screen_id)
                        })
                    {
                        bail!("Session acceptance violates authenticated contract");
                    }
                    if let Ok(mut s) = shared.lock() {
                        s.host_identity = contract.host_id.clone();
                        s.viewer_identity = viewer_id;
                        s.preferences.screens.retain(|p| {
                            contract
                                .screens
                                .iter()
                                .any(|screen| screen.screen_id == p.screen_id)
                        });
                        s.contract = Some(contract);
                        s.authenticated_key = Some(key);
                        bridge_core::sessions::clear_secret(&mut s.pin);
                        if s.remember {
                            if let Err(e) = save_viewer_session(&s) {
                                s.message =
                                    format!("Login worked but saving credentials failed: {e}");
                            } else {
                                s.remembered = saved_viewer_session(&s);
                            }
                        }
                    }
                    screens
                }

                HostMessage::SessionAccepted {
                    host_id,
                    viewer_id,
                    screen,
                } if login.is_none() => {
                    if let Ok(mut s) = shared.lock() {
                        s.host_identity = host_id;
                        s.viewer_identity = viewer_id;
                    }
                    vec![screen]
                }
                HostMessage::Accepted { .. } => bail!("Legacy session acceptance is unsupported"),
                HostMessage::Error { message, .. } => bail!("Host: {message}"),
                _ => bail!("Host sent an invalid session response"),
            };
            if login.is_none()
                && screens.first().map(|s| s.screen_id)
                    != Some(shared.lock().map(|s| s.target_screen).unwrap_or(0))
            {
                bail!("Host accepted a different screen than authenticated");
            }
            Ok::<_, anyhow::Error>((endpoint, connection, control_send, control_recv, screens))
        };
        tokio::pin!(setup);
        let (_endpoint, connection, mut control_send, control_recv, screens) = loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(()),
                result = &mut setup => break result?,
                command = commands.recv() => match command {
                    Some(Command::Disconnect) | None => { message(&shared, "Connection cancelled"); return Ok(()); }
                    _ => message(&shared, "Connection is still being established"),
                }
            }
        };
        let mut queued_commands = VecDeque::new();
        // Slots paused on this device are not attached by the host; never
        // reinstall them from a late message either.
        if let Ok(mut state) = shared.lock() {
            let paused = state
                .preferences
                .screens
                .iter()
                .filter(|slot| !slot.visible)
                .map(|slot| slot.screen_id)
                .collect::<Vec<_>>();
            if state.contract.is_some() {
                state.removed_screens.extend(paused);
            }
        }
        for screen in screens {
            if !install_during_session(
                &shared,
                screen,
                &connection,
                commands,
                &mut queued_commands,
                &mut shutdown,
            )
            .await?
            {
                connection.close(0u32.into(), b"viewer cancelled startup");
                message(&shared, "Connection cancelled");
                return Ok(());
            }
        }
        if let Ok(mut state) = shared.lock() {
            state.clock = ClockEstimate::default();
            state.connected = true;
            state.connecting = false;
            state.bytes = 0;
            state.received_frames = 0;
        }
        message(&shared, "Connected");
        let (lane_events, mut lane_notices) = mpsc::channel(64);
        let mut lane_task = Some(tokio::spawn(receive_lanes(
            connection.clone(),
            shared.clone(),
            lane_events,
        )));
        let stats_state = shared.clone();
        let diagnostic_send = tokio::spawn(bridge_core::telemetry_transport::send_batches(
            connection.clone(),
            move || {
                stats_state
                    .lock()
                    .map(|s| {
                        s.screens
                            .iter()
                            .filter(|s| s.config.instrumented)
                            .map(|s| (s.config.screen_id, s.config.generation, s.telemetry.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        ));
        let mut probes = std::collections::VecDeque::<u64>::new();
        let mut clock_timer = tokio::time::interval(Duration::from_secs(2));
        clock_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (mut control_messages, control_task) = control_reader::<_, HostMessage>(control_recv);
        let mut watchdog = tokio::time::interval(Duration::from_millis(100));
        let mut last_stats = now_us();
        let mut counters = HashMap::<ScreenId, (u64, u64, u64, u64, u64)>::new();
        let power_state = shared.clone();
        let power = bridge_core::power::PowerController::new(move |notice| {
            if let Ok(mut s) = power_state.lock() {
                s.power_notice = notice;
            }
        });
        let mut video_progress = HashMap::new();
        let mut last_preferences = String::new();
        let mut last_preferences_at = std::time::Instant::now() - Duration::from_secs(2);
        let session_result: Result<()> = async {
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    reason = connection.closed() => bail!("Host disconnected: {reason}"),
                    _=clock_timer.tick()=>{
                        let t1=now_us(); if probes.len()==8 {probes.pop_front();} probes.push_back(t1);
                        send_control(&mut control_send,&ClientMessage::ClockProbe{t1}).await?;
                    }
                    _ = watchdog.tick() => {
                        let screens = shared.lock().map(|state| state.screens.clone()).unwrap_or_default();
                        power.set_active(shared.lock().map(|s|s.effective_keep_awake()).unwrap_or(false)&&screens.iter().any(|s|!s.terminal&&s.frame.lock().map(|f|f.frames>0&&!f.closed).unwrap_or(false)));
                        if let Ok(mut s)=shared.lock(){for screen in &screens{if let Ok(frame)=screen.frame.lock(){if frame.window.width>0&&frame.window.height>0{if let Some(saved)=s.preferences.screen_mut(screen.config.screen_id){saved.window=frame.window.clone();}}}}}
                        let preferences=shared.lock().ok().filter(|s|s.contract.is_some()).map(|s|s.preferences.clone());
                        if let Some(preferences)=preferences{let snapshot=bridge_core::sessions::preferences_snapshot(&preferences)?;if snapshot!=last_preferences&&last_preferences_at.elapsed()>=Duration::from_secs(1){send_control(&mut control_send,&ClientMessage::SessionPreferences(preferences)).await?;if let Ok(mut s)=shared.lock(){if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}}last_preferences=snapshot;last_preferences_at=std::time::Instant::now();}}

                        counters.retain(|id,_|screens.iter().any(|s|s.config.screen_id==*id));
                        let stats_now=now_us(); let collect=stats_now.saturating_sub(last_stats)>=1_000_000;
                        video_progress.retain(|id, _| screens.iter().any(|s| s.config.screen_id == *id));
                        for screen in screens {
                            let progress = screen.frame.lock().ok().filter(|f| !f.closed && !screen.terminal).map(|f| bridge_core::protocol::VideoProgress {
                                screen_id: screen.config.screen_id,
                                generation: screen.config.generation,
                                decoded: f.decoded_sequence,
                                submitted: f.submitted_sequence,
                            });
                            if let Some(progress) = progress.filter(|p| p.decoded.is_some()) {
                                if video_progress.get(&progress.screen_id).is_none_or(|(last, at)| {
                                    *last != progress || stats_now.saturating_sub(*at) >= 500_000
                                }) {
                                    send_control(&mut control_send, &ClientMessage::VideoProgress(progress)).await?;
                                    video_progress.insert(progress.screen_id, (progress, stats_now));
                                }
                            }
                            if collect && screen.config.instrumented {
                                let (decoded,skipped,presented,memory,in_flight,age,engine)=screen.frame.lock().map(|s|(s.frames,s.skipped,s.presented,s.gpu_memory_bytes,s.gpu_in_flight,s.frame_age_us,s.decoder.clone())).unwrap_or_default();
                                let previous=counters.insert(screen.config.screen_id,(screen.bytes,screen.received_frames,decoded,connection.rtt().as_micros() as u64,presented)).unwrap_or_default();
                                let elapsed=stats_now.saturating_sub(last_stats) as f64/1_000_000.;let rtt=connection.rtt().as_micros() as u64;let transport=connection.stats();
                                screen.telemetry.stats(StreamStats{at_us:stats_now,bitrate_kbps:screen.bytes.saturating_sub(previous.0) as f64*8./elapsed/1000.,fps:screen.received_frames.saturating_sub(previous.1) as f64/elapsed,decoded_fps:Some(decoded.saturating_sub(previous.2) as f64/elapsed),presented_fps:Some(presented.saturating_sub(previous.4) as f64/elapsed),gpu_memory_bytes:Some(memory),gpu_in_flight:Some(in_flight),frame_age_us:age,engine:Some(engine),frames:screen.received_frames,dropped:skipped+screen.host_frames_dropped,rtt_us:rtt,jitter_us:rtt.abs_diff(previous.3),lost_packets:transport.path.lost_packets,congestion_events:transport.path.congestion_events,..Default::default()});
                                let clock=shared.lock().map(|s|s.clock).unwrap_or_default();screen.telemetry.expire(stats_now as i64+clock.offset_us,"Stage did not conclude within five seconds");
                            }
                            if screen.terminal && !screen.cleanup_reported && screen.frame.lock().map(|f|f.cleanup_complete).unwrap_or(false){
                                if let Ok(mut state)=shared.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.config.screen_id==screen.config.screen_id&&s.config.generation==screen.config.generation){s.cleanup_reported=true;}}
                                send_control(&mut control_send,&ClientMessage::VideoStopped{screen_id:screen.config.screen_id,generation:screen.config.generation}).await?;
                            }
                            if !screen.terminal && screen.frame.lock().map(|f|f.frames>0).unwrap_or(false){update_client_screen_status(&shared,screen.config.screen_id,screen.config.generation,"Streaming video".into());}
                            if screen.removing { continue; }
                            let (closed, user_closed, error) = screen.frame.lock().map(|frame| (frame.closed, frame.user_closed, frame.error.clone())).unwrap_or((true, false, Some("Frame state unavailable".into())));
                            if user_closed {
                                request_remove_screen(&shared, &mut control_send, screen.config.screen_id).await?;
                            } else if closed && !screen.terminal {
                                end_client_lane(&shared, screen.config.screen_id, screen.config.generation,
                                    error.unwrap_or_else(|| "The display backend stopped".into()));
                            }
                        }
                        if collect {last_stats=stats_now;}
                    }
                    command = async {
                        if let Some(command) = queued_commands.pop_front() { Some(command) } else { commands.recv().await }
                    } => {
                        match command {
                            Some(Command::Diagnostics(id,enabled))=>{
                                if let Ok(mut state)=shared.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.config.screen_id==id){s.stats_view.open=enabled;s.stats_view.frozen=None;}}
                                send_control(&mut control_send,&ClientMessage::Diagnostics{screen_id:id,enabled}).await?;
                            }
                            Some(Command::Burst(id))=>{send_control(&mut control_send,&ClientMessage::TraceBurst{screen_id:id}).await?;}
                            Some(Command::Disconnect) | None => break,
                            Some(Command::RemoveScreen(screen_id)) => {
                                request_remove_screen(&shared, &mut control_send, screen_id).await?;
                            }
                            Some(Command::RetryScreen(id)) => {
                                if !shared.lock().map(|s|s.screens.iter().any(|screen|screen.config.screen_id==id && (!screen.terminal || screen.frame.lock().map(|f|f.cleanup_complete).unwrap_or(false)))).unwrap_or(false){continue;}
                                if let Ok(mut state)=shared.lock(){state.retry_requested=true;state.message="Reconnecting this screen…".into();}
                                break;
                            }
                            Some(Command::AttachScreen(screen_id)) => {
                                let session = shared.lock().map(|mut state| {
                                    state.removed_screens.remove(&screen_id);
                                    if let Some(slot) = state.preferences.screen_mut(screen_id) { slot.visible = true; }
                                    state.contract.is_some() && state.screen(screen_id).is_none()
                                }).unwrap_or(false);
                                if session {
                                    send_control(&mut control_send, &ClientMessage::AttachScreen { request_id: next_request_id(&shared), screen_id }).await?;
                                }
                            }
                            Some(Command::Present(screen_id)) => {
                                let target = shared.lock().ok().and_then(|state| state.screen(screen_id).filter(|s| !s.terminal && !s.removing).map(|s| (s.video_window.clone(), state.display(screen_id), s.diagnostics.clone())));
                                if let Some((window, display, diagnostics)) = target {
                                    if let Err(error) = window.set_presentation(display).await {
                                        diagnostics.record("window", format!("Display settings unavailable: {error}"));
                                    }
                                }
                            }
                            Some(Command::Connect(_, _, _)) => message(&shared, "Already connected"),
                        }
                    }
                    lane = async { lane_task.as_mut().expect("Media task owned until completion").await } => {
                        // The output has been consumed: cleanup must not poll it again.
                        lane_task.take();
                        match lane {
                            Ok(()) => bail!("The media connection closed"),
                            Err(error) => bail!("Media lane manager stopped: {error}"),
                        }
                    }
                    notice = lane_notices.recv() => {
                        match notice {
                            Some(LaneNotice::Opened(screen_id, generation, lane)) => update_client_screen_status(&shared, screen_id, generation, format!("{lane:?} lane opened · waiting for frames")),
                            Some(LaneNotice::FirstFrame(screen_id, generation)) => update_client_screen_status(&shared, screen_id, generation, "Video received · waiting for decoder/display".into()),
                            Some(LaneNotice::Failed(screen_id, generation, lane, error)) => if screen_id == 0 { message(&shared, format!("Rejected media lane: {error}")); } else if lane == MediaLane::Video { end_client_lane(&shared, screen_id, generation, format!("Video lane failed: {error}")); },
                            Some(LaneNotice::Closed(screen_id, generation, lane)) => if lane == MediaLane::Video { end_client_lane(&shared, screen_id, generation, "Video lane closed".into()); },
                            None => {},
                        }
                    }
                    response = control_messages.recv() => {
                        match response.ok_or_else(|| anyhow::anyhow!("Host control channel ended"))?? {
                            HostMessage::ScreenAdded {screen,..}=>{
                                let allowed=shared.lock().map(|s|s.contract.as_ref().is_some_and(|contract|contract.screens.iter().any(|saved|saved.screen_id==screen.screen_id))).unwrap_or(false);
                                if !allowed{bail!("Additional screen outside authenticated session");}
                                if !install_during_session(&shared,screen,&connection,commands,&mut queued_commands,&mut shutdown).await?{break;}
                            },
                            HostMessage::SessionUpdated(contract)=>{contract.validate()?;if let Ok(mut s)=shared.lock(){if s.contract.as_ref().is_none_or(|c|c.session_id!=contract.session_id||c.host_id!=contract.host_id||contract.revision<c.revision){bail!("Session update identity/revision mismatch");}s.preferences.screens.retain(|p|contract.screens.iter().any(|screen|screen.screen_id==p.screen_id));s.contract=Some(contract);if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}}},
                            HostMessage::PersistentSessionAccepted{..}=>bail!("Unexpected duplicate persistent session acceptance"),
                            HostMessage::ScreenSettingsApplied { request_id, screen } => {
                                if !shared.lock().map(|s|s.screens.iter().any(|current|current.config.screen_id==screen.screen_id)).unwrap_or(false){bail!("Settings belong to an unattached screen");}
                                let _ = request_id;
                                match install_during_session(&shared, screen.clone(), &connection, commands, &mut queued_commands, &mut shutdown).await {
                                    Ok(false) => break,
                                    // Generation and apply events belong in the screen's log, not the session row.
                                    Ok(true) => { if let Ok(state) = shared.lock() { if let Some(entry) = state.screen(screen.screen_id) { entry.diagnostics.record("config", format!("Host settings applied · stream generation {}", screen.generation)); } } },
                                    Err(error) => message(&shared, format!("Could not apply {} settings: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenRemoved { request_id, screen_id } => {
                                let _ = request_id;
                                // A pause we asked for stays excluded; a screen the host took
                                // away (removed, session deactivated) may come back later.
                                let paused = shared.lock().map(|s| !s.preferences.visible(screen_id)).unwrap_or(false);
                                remove_client_screen(&shared, screen_id).await;
                                if !paused { if let Ok(mut state) = shared.lock() { state.removed_screens.remove(&screen_id); } }
                            },
                            HostMessage::StreamStatus { screen_id, generation, lane, state: stream_state, message: detail } => {
                                if lane == MediaLane::Video && matches!(stream_state, MediaState::Failed | MediaState::Stopped) {
                                    end_client_lane(&shared, screen_id, generation, detail);
                                } else if lane == MediaLane::Video {
                                    update_client_screen_status(&shared, screen_id, generation, detail);
                                } else {
                                    if let Ok(state) = shared.lock() {
                                        if let Some(screen) = state.screens.iter().find(|screen| screen.config.screen_id == screen_id && screen.config.generation == generation) {
                                            screen.diagnostics.record("stream", format!("{lane:?} {stream_state:?}: {detail}"));
                                        }
                                    }
                                }
                            }
                            HostMessage::Stats { screen_id, generation, frames_sent, frames_dropped, bitrate_kbps } => {
                                if let Ok(mut state) = shared.lock() {
                                    if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id && screen.config.generation == generation && !screen.terminal && !screen.removing) {
                                        screen.host_frames_sent = frames_sent;
                                        screen.host_frames_dropped = frames_dropped;
                                        screen.bitrate_kbps = bitrate_kbps;
                                        if frames_dropped > 0 {

                                        }
                                    }
                                }
                            }
                            HostMessage::Error { request_id, screen_id, message: detail } => {
                                let _ = request_id;
                                if let Ok(mut state) = shared.lock() {
                                    state.message = format!("Host: {detail}");
                                    if let Some(screen_id) = screen_id {
                                        if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id) {
                                            screen.status = detail.clone();
                                            screen.diagnostics.record("error", &detail);
                                        }
                                    } else {
                                        state.diagnostics.record("host", &detail);
                                    }
                                }
                            }
                            HostMessage::ClockReply{t1,t2,t3}=>{
                                let t4=now_us();
                                if let Some(index)=probes.iter().position(|p|*p==t1){probes.remove(index);if let Some(sample)=ClockEstimate::from_probe(t1,t2,t3,t4){if let Ok(mut state)=shared.lock(){state.clock.update(sample);}}}
                            }
                            HostMessage::Failure(failure) => {
                                if failure.scope==bridge_core::failure::FailureScope::Video {end_client_lane(&shared,failure.screen_id,failure.generation,format!("{}: {}",failure.origin,failure.message));}
                                else if failure.scope==bridge_core::failure::FailureScope::Session {bail!("{}",failure.message);}
                                else {message(&shared,failure.message);}
                            },
                            HostMessage::SessionAccepted { .. } => message(&shared,"Duplicate session acceptance ignored"),
                            HostMessage::Accepted { .. } => message(&shared, "Host sent a second session acceptance; ignoring it"),
                        }
                    }
                }
            }
            Ok(())
        }.await;
        power.set_active(false);
        clear_client_session(&shared);
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            send_control(&mut control_send, &ClientMessage::Stop),
        )
        .await;
        diagnostic_send.abort();
        let _ = diagnostic_send.await;
        control_task.abort();
        let _ = control_task.await;
        stop_lane_manager(lane_task).await;
        connection.close(0u32.into(), b"viewer disconnected");
        clear_client_session(&shared);
        if session_result.is_ok() {
            let cause = shared.lock().ok().and_then(|s| s.terminal_reason.clone());
            message(&shared, cause.unwrap_or_else(|| "Disconnected".into()));
        }
        session_result
    }

    async fn stop_lane_manager(task: Option<tokio::task::JoinHandle<()>>) {
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    fn update_client_screen_status(
        shared: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        generation: StreamGeneration,
        status: String,
    ) {
        if let Ok(mut state) = shared.lock() {
            if let Some(screen) = state.screens.iter_mut().find(|screen| {
                screen.config.screen_id == screen_id
                    && screen.config.generation == generation
                    && !screen.terminal
                    && !screen.removing
            }) {
                if screen.status != status {
                    screen.status = status.clone();
                    screen.diagnostics.record("stream", &status);
                }
            }
        }
    }

    fn saved_viewer_session(s: &State) -> Option<bridge_core::sessions::SavedViewerSession> {
        Some(bridge_core::sessions::SavedViewerSession {
            contract: s.contract.clone()?,
            viewer_id: s.stable_viewer.clone(),
            key: s
                .authenticated_key
                .clone()
                .or_else(|| s.remembered.as_ref().map(|s| s.key.clone()))?,
            preferences: s.preferences.clone(),
            auto_connect: s.auto_connect,
        })
    }
    /// Persist this entry's preferences (pause, display settings) when it has a saved login.
    fn save_preferences(s: &mut State) {
        if s.contract.is_some() && s.remember {
            if let Err(e) = save_viewer_session(s) {
                s.message = format!("Session save failed: {e}");
            } else {
                let preferences = s.preferences.clone();
                if let Some(saved) = &mut s.remembered {
                    saved.preferences = preferences;
                }
            }
        }
    }
    fn save_viewer_session(s: &State) -> Result<()> {
        if let Some(saved) = saved_viewer_session(s) {
            if s.remember {
                bridge_core::sessions::remember_viewer(saved)?;
            } else {
                bridge_core::sessions::forget_viewer(&saved.contract.session_id)?;
            }
        }
        Ok(())
    }
    fn initial_state() -> State {
        State {
            clock: ClockEstimate::default(),
            hosts: Vec::new(),
            target_screen: 1,
            host_label: String::new(),
            host_identity: String::new(),
            viewer_identity: 0,
            pin: String::new(),
            username: String::new(),
            stable_viewer: bridge_core::sessions::new_id().expect("System randomness unavailable"),
            remembered: None,
            authenticated_key: None,
            contract: None,
            preferences: Default::default(),
            remember: false,
            auto_connect: false,
            auto_suspended: false,
            power_notice: String::new(),
            screens: Vec::new(),
            removed_screens: HashSet::new(),
            next_request_id: 1,
            connected: false,
            connecting: false,
            message: "Discovering Macs…".into(),
            terminal_reason: None,
            retry_requested: false,
            last_endpoint: None,
            diagnostics: Arc::new(Diagnostics::new("viewer")),
            bytes: 0,
            received_frames: 0,
        }
    }

    pub fn main() {
        let state = Arc::new(Mutex::new(initial_state()));
        let workspace = Arc::new(Mutex::new(Workspace::default()));
        match bridge_core::sessions::ViewerStore::load() {
            Ok(store) => {
                if let Ok(mut s) = state.lock() {
                    s.stable_viewer = store.viewer_id;
                }
                for saved in store.sessions {
                    add_local_session(&workspace, &state, Some(saved));
                }
            }
            Err(e) => message(&state, format!("Saved logins unavailable: {e}")),
        }

        let (shutdown, _shutdown_rx) = watch::channel(false);
        let browse_state = state.clone();
        let browse_workspace = workspace.clone();
        let discovery_stop = Arc::new(AtomicBool::new(false));
        let browse_stop = discovery_stop.clone();
        let discovery_thread = std::thread::spawn(move || match browse(browse_stop.clone()) {
            Ok(found) => {
                while !browse_stop.load(Ordering::Acquire) {
                    match found.recv_timeout(Duration::from_millis(250)) {
                        Ok(event) => {
                            if let Ok(mut state) = browse_state.lock() {
                                state.discovery_event(event.clone());
                            }
                            let entries = browse_workspace
                                .lock()
                                .map(|ws| ws.entries.clone())
                                .unwrap_or_default();
                            for entry in entries {
                                if let Ok(mut state) = entry.state.lock() {
                                    state.discovery_event(event.clone());
                                }
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(_) => break,
                    }
                }
            }
            Err(error) => message(&browse_state, format!("Discovery unavailable: {error}")),
        });
        let app_shutdown = shutdown.clone();
        let app_workspace = workspace.clone();
        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx: &mut App| {
                gpui_kit::init(cx);
                cx.on_app_quit(move |_| {
                    let _ = app_shutdown.send(true);
                    async {}
                })
                .detach();
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                let controls = open_controls(cx, app_workspace.clone(), state.clone())
                    .expect("Cannot open AirBridge controls");
                let _ = controls;
                state.lock().unwrap().diagnostics.record(
                    "viewer",
                    "Independent native video window and GPUI controls opened",
                );
                cx.activate(true);
            });
        let _ = shutdown.send(true);
        let entries = workspace
            .lock()
            .map(|ws| {
                ws.entries
                    .iter()
                    .chain(&ws.retiring)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for entry in &entries {
            entry.shutdown.send_replace(true);
        }
        for entry in entries {
            if let Some(task) = entry.thread.lock().ok().and_then(|mut t| t.take()) {
                let _ = task.join();
            }
        }
        discovery_stop.store(true, Ordering::Release);
        let _ = discovery_thread.join();
        VideoWindow::shutdown_backend();
    }
    include!("tests.rs");
}

fn main() {
    #[cfg(target_os = "linux")]
    if linux::worker_entry() {
        return;
    }
    app::main();
}
