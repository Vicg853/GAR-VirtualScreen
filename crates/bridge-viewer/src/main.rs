#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod linux;
#[cfg(target_os = "linux")]
mod native_video;
#[cfg(any(target_os = "linux", test))]
mod resource_lifecycle;
#[cfg(target_os = "linux")]
mod stream_engine;

mod app {
    use super::linux::{VideoFrame, VideoWindow};
    use anyhow::{Result, bail};
    use bridge_core::{
        config::{
            Acceleration, Codec, CodecOption, Preset, VideoSettings, built_in_presets,
            load_custom_presets, save_custom_presets,
        },
        diagnostics::Diagnostics,
        discovery::{DiscoveredHost, DiscoveryEvent, browse},
        lifecycle::{ScreenNavigation, ScreenView},
        protocol::{
            ChangeScreenSettingsRequest, ClientHello, ClientMessage, HostMessage, MAX_SCREENS,
            MediaKind, MediaLane, MediaState, MediaStreamHello, RequestId, ScreenConfig, ScreenId,
            StreamGeneration, VERSION, control_reader, receive_debug_media_or_end, receive_json,
            receive_media_or_end, send_control,
        },
        security::{authenticate_viewer_screen, client_endpoint},
        telemetry::{
            ClockEstimate, Outcome, Stage, StatsView, StreamStats, Telemetry, TimingView,
            TraceToken, now_us,
        },
    };
    use gpui_kit::component::{
        ActiveTheme, Disableable, Selectable, Sizable, TitleBar,
        button::{Button, ButtonVariants},
        form::{Field, Form},
        input::{Input, InputEvent, InputState},
    };
    use gpui_kit::{
        AnyWindowHandle, App, Bounds, Context, Entity, MouseButton, ResizeEdge, Subscription,
        Window, WindowBounds, WindowDecorations, WindowOptions, bounds, div, point, prelude::*, px,
        size,
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
        Connect(SocketAddr, String, Option<VideoSettings>),
        Disconnect,
        ChangeScreenSettings(ScreenId, u64, VideoSettings),
        RemoveScreen(ScreenId),
        RetryScreen(ScreenId),
        Diagnostics(ScreenId, bool),
        Burst(ScreenId),
    }

    enum PendingAction {
        Settings(ScreenId, u64),
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
        requested_revision: u64,
        desired_settings: VideoSettings,
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
        selected_host: usize,
        target_screen: u32,
        target_input: String,
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
        manual_address: String,
        settings: VideoSettings,
        override_host: bool,
        screens: Vec<ClientScreen>,
        removed_screens: HashSet<ScreenId>,
        navigation: ScreenNavigation,
        next_request_id: u64,
        connected: bool,
        connecting: bool,
        adding: bool,
        presets: Vec<Preset>,
        selected_preset: usize,
        message: String,
        terminal_reason: Option<String>,
        retry_requested: bool,
        last_endpoint: Option<SocketAddr>,
        show_codecs: bool,
        show_logs: bool,
        diagnostics: Arc<Diagnostics>,
        bytes: u64,
        received_frames: u64,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ControlsPage {
        Connect,
        Connecting,
        List,
        Details,
    }

    impl State {
        fn auto_endpoint(&self) -> Option<SocketAddr> {
            if !self.auto_connect || self.auto_suspended || !self.remember {
                return None;
            }
            let saved = self.remembered.as_ref()?;
            self.hosts
                .iter()
                .filter(|host| host.host_id == saved.contract.host_id)
                .flat_map(|host| host.addresses.iter())
                .copied()
                .next()
        }
        fn select_host(&mut self, name: &str) {
            if let Some(index) = self.hosts.iter().position(|host| host.name == name) {
                self.selected_host = index;
                self.target_screen = self.hosts[index].screen_id;
                self.target_input = self.target_screen.to_string();
                self.message = format!("Selected {name}");
            } else {
                self.message = format!("Host no longer available: {name}");
            }
            self.diagnostics.record("viewer", &self.message);
        }

        fn discovery_event(&mut self, event: DiscoveryEvent) {
            match event {
                DiscoveryEvent::Found(host) => {
                    if let Some(existing) =
                        self.hosts.iter_mut().find(|entry| entry.name == host.name)
                    {
                        *existing = host;
                    } else {
                        if self.hosts.is_empty()
                            && !self.connected
                            && !self.connecting
                            && self.manual_address.is_empty()
                        {
                            self.target_screen = host.screen_id;
                            self.target_input = host.screen_id.to_string();
                        }
                        self.diagnostics
                            .record("discovery", format!("Found {}", host.name));
                        if !self.connected && !self.connecting {
                            self.message =
                                format!("Found {} · enter the PIN to connect", host.name);
                        }
                        self.hosts.push(host);
                    }
                }
                DiscoveryEvent::Lost(name) => {
                    let selected = self
                        .hosts
                        .get(self.selected_host)
                        .map(|host| host.name.clone());
                    self.hosts.retain(|host| host.name != name);
                    self.selected_host = selected
                        .and_then(|name| self.hosts.iter().position(|host| host.name == name))
                        .unwrap_or_default();
                    self.diagnostics
                        .record("discovery", format!("Host disappeared: {name}"));
                    if !self.connected && !self.connecting {
                        self.message = format!("Host disappeared: {name}");
                    }
                }
            }
        }

        fn page(&self) -> ControlsPage {
            if self.connecting {
                ControlsPage::Connecting
            } else if !self.connected {
                ControlsPage::Connect
            } else if self.navigation.view == ScreenView::Details
                && self
                    .screens
                    .iter()
                    .any(|s| Some(s.config.screen_id) == self.navigation.selected)
            {
                ControlsPage::Details
            } else {
                ControlsPage::List
            }
        }
    }

    fn message(state: &Arc<Mutex<State>>, text: impl Into<String>) {
        let text = text.into();
        if let Ok(mut state) = state.lock() {
            state.diagnostics.record("viewer", &text);
            state.message = text;
        }
    }

    fn persist_selected_settings(state: &mut State) -> Option<(ScreenId, u64, VideoSettings)> {
        let screen_id = state.navigation.selected?;
        let screen = state
            .screens
            .iter_mut()
            .find(|screen| screen.config.screen_id == screen_id)?;
        if screen.removing {
            return None;
        }
        screen.requested_revision = screen
            .requested_revision
            .max(screen.config.revision)
            .saturating_add(1);
        screen.desired_settings = state.settings.clone();
        screen.status = "Applying settings · rebuilding media pipeline…".into();
        Some((screen_id, screen.requested_revision, state.settings.clone()))
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
        last_selected: Option<u64>,
        selected: Option<u64>,
        next_id: u64,
    }
    fn add_local_screen(workspace: &Arc<Mutex<Workspace>>, catalog: &Arc<Mutex<State>>) {
        add_local_session(workspace, catalog, None);
    }
    fn add_local_session(
        workspace: &Arc<Mutex<Workspace>>,
        catalog: &Arc<Mutex<State>>,
        saved: Option<bridge_core::sessions::SavedViewerSession>,
    ) {
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
            return;
        }
        ws.next_id += 1;
        let id = ws.next_id;
        let mut state = initial_state(snapshot.presets);
        state.hosts = snapshot.hosts;
        state.stable_viewer = snapshot.stable_viewer;
        state.target_screen = state.hosts.first().map(|h| h.screen_id).unwrap_or(1);
        state.target_input = state.target_screen.to_string();
        state.message =
            "Select a host and log in, or leave account blank to use a screen PIN".into();
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
        ws.entries.push(LocalScreen {
            id,
            state,
            commands,
            shutdown,
            thread: Arc::new(Mutex::new(Some(task))),
        });
        ws.selected = Some(id);
        ws.last_selected = Some(id);
    }
    fn remove_local_screen(workspace: &Arc<Mutex<Workspace>>, id: u64) {
        let entry = workspace.lock().ok().and_then(|mut ws| {
            let index = ws.entries.iter().position(|e| e.id == id)?;
            if ws.selected == Some(id) {
                ws.selected = None;
            }
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
    struct ViewerView {
        workspace: Arc<Mutex<Workspace>>,
        catalog: Arc<Mutex<State>>,
        state: Arc<Mutex<State>>,
        commands: mpsc::Sender<Command>,
        inputs: [Entity<InputState>; 4],
        input_owner: Option<u64>,
        input_values: [String; 4],
        _input_subscriptions: Vec<Subscription>,
    }

    fn resize_from_border(event: &gpui_kit::MouseDownEvent, window: &mut Window) {
        let point = event.position;
        let size = window.window_bounds().get_bounds().size;
        let border = px(7.0);
        let left = point.x < border;
        let right = point.x > size.width - border;
        let top = point.y < border;
        let bottom = point.y > size.height - border;
        let edge = match (left, right, top, bottom) {
            (true, _, true, _) => Some(ResizeEdge::TopLeft),
            (_, true, true, _) => Some(ResizeEdge::TopRight),
            (true, _, _, true) => Some(ResizeEdge::BottomLeft),
            (_, true, _, true) => Some(ResizeEdge::BottomRight),
            (true, _, _, _) => Some(ResizeEdge::Left),
            (_, true, _, _) => Some(ResizeEdge::Right),
            (_, _, true, _) => Some(ResizeEdge::Top),
            (_, _, _, true) => Some(ResizeEdge::Bottom),
            _ => None,
        };
        if let Some(edge) = edge {
            window.start_window_resize(edge);
        }
    }

    fn setting_button(
        label: &'static str,
        id: &'static str,
        shared: Arc<Mutex<State>>,
        commands: mpsc::Sender<Command>,
        action: fn(&mut State),
        cx: &mut Context<ViewerView>,
    ) -> impl IntoElement {
        Button::new(id)
            .label(label)
            .primary()
            .small()
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Ok(mut state) = shared.lock() {
                    if state.screens.iter().any(|s| {
                        Some(s.config.screen_id) == state.navigation.selected && s.removing
                    }) {
                        return;
                    }
                    action(&mut state);
                    state.override_host = true;
                    let apply = if label == "Save custom preset" {
                        None
                    } else {
                        persist_selected_settings(&mut state)
                    };
                    if label != "Save custom preset" {
                        state.message = if state.connected && state.navigation.selected.is_some() {
                            format!("{label} updated · applying to this screen")
                        } else {
                            format!("{label} updated · applies on the next connection")
                        };
                    }
                    state.diagnostics.record("settings", &state.message);
                    if let Some((screen_id, revision, settings)) = apply {
                        let _ = commands
                            .try_send(Command::ChangeScreenSettings(screen_id, revision, settings));
                    }
                }
                cx.notify();
            }))
    }

    impl ViewerView {
        fn inspect_stats(&mut self, id: ScreenId, cx: &mut Context<Self>) {
            if let Ok(mut state) = self.state.lock() {
                let Some(index) = state.screens.iter().position(|s| s.config.screen_id == id)
                else {
                    return;
                };
                if !state.screens[index].stats_view.open {
                    if let Some(Err(error)) =
                        Some(self.commands.try_send(Command::Diagnostics(id, true)))
                    {
                        state.message = format!("Could not enable statistics: {error}");
                        state.diagnostics.record("stats", &state.message);
                        cx.notify();
                        return;
                    }
                    state.screens[index].stats_view.reset();
                    state.screens[index].stats_view.open = true;
                }
            } else {
                return;
            }
            let shared = self.state.clone();
            let read = shared.clone();
            let commands = self.commands.clone();
            let source = bridge_ui::InspectionSource::new(
                move || {
                    let mut state = read.lock().ok()?;
                    let screen = state
                        .screens
                        .iter_mut()
                        .find(|s| s.config.screen_id == id)?;
                    if !screen.stats_view.open {
                        return None;
                    }
                    let telemetry = screen.telemetry.clone();
                    screen.stats_view.refresh(&telemetry);
                    let title = format!(
                        "Screen {id} · {} · generation {}",
                        screen.config.name, screen.config.generation
                    );
                    Some(bridge_ui::InspectionData::new(
                        title,
                        screen.stats_view.table(&telemetry),
                        screen.stats_view.lines(&telemetry),
                        screen.stats_view.mode,
                        screen.stats_view.frozen.is_some(),
                    ))
                },
                move |action| {
                    if let Ok(mut state) = shared.lock() {
                        if let Some(screen) =
                            state.screens.iter_mut().find(|s| s.config.screen_id == id)
                        {
                            if !screen.stats_view.open {
                                return;
                            }
                            let telemetry = screen.telemetry.clone();
                            match action {
                                bridge_ui::InspectionAction::Close => {
                                    screen.stats_view.open = false;
                                    screen.stats_view.reset();
                                }
                                bridge_ui::InspectionAction::Frames => {
                                    screen.stats_view.mode = TimingView::Frames
                                }
                                bridge_ui::InspectionAction::Averages => {
                                    screen.stats_view.mode = TimingView::Averages
                                }
                                bridge_ui::InspectionAction::Freeze => {
                                    screen.stats_view.frozen = if screen.stats_view.frozen.is_some()
                                    {
                                        None
                                    } else {
                                        Some(telemetry.snapshot())
                                    };
                                    screen.stats_view.freeze_after = None;
                                }
                                bridge_ui::InspectionAction::Burst => {
                                    screen.stats_view.frozen = None;
                                    screen.stats_view.freeze_after = Some(
                                        telemetry.snapshot().averages[Stage::Capture as usize]
                                            .count
                                            + 20,
                                    );
                                }
                            }
                        } else {
                            return;
                        }
                        if matches!(action, bridge_ui::InspectionAction::Close) {
                            if let Some(Err(error)) =
                                Some(commands.try_send(Command::Diagnostics(id, false)))
                            {
                                state.message = format!("Could not disable statistics: {error}");
                                state.diagnostics.record("stats", &state.message);
                            }
                        }
                        if matches!(action, bridge_ui::InspectionAction::Burst) {
                            let _ = Some(commands.try_send(Command::Burst(id)));
                        }
                    }
                },
            );
            if let Err(error) = bridge_ui::open_inspector(
                format!("viewer:{}:{id}", self.input_owner.unwrap_or(0)),
                source.clone(),
                cx,
            ) {
                source.close();
                message(
                    &self.state,
                    format!("Cannot open statistics inspector: {error:#}"),
                );
            }
        }
    }

    impl Render for ViewerView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let last_selected = self.workspace.lock().ok().and_then(|ws| ws.last_selected);
            let (entries, selected) = self
                .workspace
                .lock()
                .map(|ws| (ws.entries.clone(), ws.selected))
                .unwrap_or_default();
            if selected.is_none() {
                let rows = entries
                    .iter()
                    .map(|entry| {
                        let id = entry.id;
                        let state = entry.state.lock().unwrap();
                        let summary = format!(
                            "{} · {}",
                            state
                                .contract
                                .as_ref()
                                .map(|c| format!("Session {}", c.username))
                                .unwrap_or_else(|| format!("Connection {id}")),
                            state.message
                        );
                        Button::new(("local-screen", id as usize))
                            .w_full()
                            .min_w_0()
                            .label(summary.clone())
                            .tooltip(summary)
                            .outline()
                            .small()
                            .selected(last_selected == Some(id))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Ok(mut ws) = this.workspace.lock() {
                                    ws.selected = Some(id);
                                }
                                this.refresh(window, cx);
                            }))
                    })
                    .collect::<Vec<_>>();
                return div()
                    .size_full()
                    .min_h_0()
                    .bg(cx.theme().background)
                    .text_color(cx.theme().foreground)
                    .flex()
                    .flex_col()
                    .on_mouse_down(MouseButton::Left, |event, window, _| {
                        resize_from_border(event, window)
                    })
                    .child(TitleBar::new().child("AirBridge · Sessions"))
                    .child(
                        div()
                            .id("local-list-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .children(rows)
                            .child(
                                Button::new("add-local-screen")
                                    .label("+ Add host session")
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        add_local_screen(&this.workspace, &this.catalog);
                                        this.refresh(window, cx);
                                    })),
                            ),
                    )
                    .into_any_element();
            }
            let state = self.state.lock().unwrap().clone();
            let page = state.page();
            let commands = self.commands.clone();
            let active_screen = state
                .screens
                .iter()
                .find(|screen| Some(screen.config.screen_id) == state.navigation.selected)
                .cloned();
            let frame_error = active_screen
                .as_ref()
                .and_then(|screen| screen.frame.lock().ok())
                .and_then(|frame| frame.error.clone());
            let hosts = state
                .hosts
                .iter()
                .enumerate()
                .map(|(index, host)| {
                    let selected = index == state.selected_host;
                    let host_name = host.name.clone();
                    Button::new(gpui_kit::SharedString::from(format!("host:{}", host.name)))
                        .w_full()
                        .min_w_0()
                        .label(format!("{}{}", if selected { "● " } else { "" }, host.name))
                        .outline()
                        .small()
                        .selected(selected)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Ok(mut state) = this.state.lock() {
                                state.select_host(&host_name);
                            }
                            this.refresh(window, cx);
                        }))
                })
                .collect::<Vec<_>>();
            let screen_cards = state
                .screens
                .iter()
                .map(|screen| {
                    let shared = self.state.clone();
                    let screen_id = screen.config.screen_id;
                    let selected = Some(screen_id) == state.navigation.selected;
                    Button::new(("viewer-screen", screen_id as usize))
                        .w_full()
                        .min_w_0()
                        .label(format!(
                            "{}{} · {}",
                            if selected { "● " } else { "" },
                            screen.config.name,
                            screen.status
                        ))
                        .outline()
                        .small()
                        .selected(selected)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                state.navigation.open(screen_id);
                                state.show_codecs = false;
                                state.show_logs = false;
                                state.navigation.view = ScreenView::Details;
                                if let Some((settings, name)) = state
                                    .screens
                                    .iter()
                                    .find(|s| s.config.screen_id == screen_id)
                                    .map(|screen| {
                                        (
                                            screen.desired_settings.clone(),
                                            screen.config.name.clone(),
                                        )
                                    })
                                {
                                    state.settings = settings;
                                    state.message = format!("Selected {name}");
                                }
                            }
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>();
            let presets = state
                .presets
                .iter()
                .enumerate()
                .map(|(index, preset)| {
                    let shared = self.state.clone();
                    let commands = commands.clone();
                    Button::new(gpui_kit::SharedString::from(format!(
                        "preset:{}",
                        preset.name
                    )))
                    .label(format!(
                        "{}{}",
                        if index == state.selected_preset {
                            "● "
                        } else {
                            ""
                        },
                        preset.name
                    ))
                    .outline()
                    .small()
                    .selected(index == state.selected_preset)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Ok(mut state) = shared.lock() {
                            if state.screens.iter().any(|s| {
                                Some(s.config.screen_id) == state.navigation.selected && s.removing
                            }) {
                                return;
                            }
                            state.selected_preset = index;
                            state.settings = state.presets[index].settings.clone();
                            state.override_host = true;
                            let apply = persist_selected_settings(&mut state);
                            state.message =
                                if state.connected && state.navigation.selected.is_some() {
                                    format!(
                                        "{} preset · applying to this screen",
                                        state.presets[index].name
                                    )
                                } else {
                                    format!(
                                        "{} preset selected for the next connection",
                                        state.presets[index].name
                                    )
                                };
                            state.diagnostics.record("settings", &state.message);
                            if let Some((screen_id, revision, settings)) = apply {
                                let _ = commands.try_send(Command::ChangeScreenSettings(
                                    screen_id, revision, settings,
                                ));
                            }
                        }
                        cx.notify();
                    }))
                })
                .collect::<Vec<_>>();
            let connect_state = self.state.clone();
            let connect_commands = self.commands.clone();
            let codec_panel = if state.show_codecs
                && !active_screen.as_ref().is_some_and(|s| s.removing)
            {
                let shared = self.state.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().secondary)
                    .child("Decoder: Vulkan Video when supported, software fallback")
                    .child("Codec: H.264")
                    .child(format!(
                        "Codec preference: {:?}",
                        state.settings.preferred_codec
                    ))
                    .child(format!(
                        "Profile: {:?} · low latency rate control: {}",
                        state.settings.h264_profile, state.settings.low_latency_rate_control
                    ))
                    .child(format!(
                        "{}×{} · {} fps · {} kbps · keyframe every {} frames",
                        state.settings.width,
                        state.settings.height,
                        state.settings.fps,
                        state.settings.bitrate_kbps,
                        state.settings.keyframe_interval
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(setting_button(
                                "Rotate",
                                "rotate",
                                shared.clone(),
                                commands.clone(),
                                |s| std::mem::swap(&mut s.settings.width, &mut s.settings.height),
                                cx,
                            ))
                            .child(setting_button(
                                "FPS −",
                                "fps-minus",
                                shared.clone(),
                                commands.clone(),
                                |s| s.settings.fps = s.settings.fps.saturating_sub(5).max(5),
                                cx,
                            ))
                            .child(setting_button(
                                "FPS +",
                                "fps-plus",
                                shared.clone(),
                                commands.clone(),
                                |s| s.settings.fps = (s.settings.fps + 5).min(240),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(setting_button(
                                "Bitrate −",
                                "bitrate-minus",
                                shared.clone(),
                                commands.clone(),
                                |s| {
                                    s.settings.bitrate_kbps =
                                        s.settings.bitrate_kbps.saturating_sub(1000).max(250)
                                },
                                cx,
                            ))
                            .child(setting_button(
                                "Bitrate +",
                                "bitrate-plus",
                                shared.clone(),
                                commands.clone(),
                                |s| {
                                    s.settings.bitrate_kbps =
                                        (s.settings.bitrate_kbps + 1000).min(100_000)
                                },
                                cx,
                            ))
                            .child(setting_button(
                                "Keyframe −",
                                "keyframe-minus",
                                shared.clone(),
                                commands.clone(),
                                |s| {
                                    s.settings.keyframe_interval =
                                        s.settings.keyframe_interval.saturating_sub(10).max(1)
                                },
                                cx,
                            ))
                            .child(setting_button(
                                "Keyframe +",
                                "keyframe-plus",
                                shared.clone(),
                                commands.clone(),
                                |s| {
                                    s.settings.keyframe_interval =
                                        (s.settings.keyframe_interval + 10).min(600)
                                },
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(setting_button(
                                "Cycle codec mode",
                                "codec-mode",
                                shared.clone(),
                                commands.clone(),
                                |s| s.settings.cycle_codec_preference(),
                                cx,
                            ))
                            .child(setting_button(
                                "Cycle H.264 profile",
                                "profile",
                                shared.clone(),
                                commands.clone(),
                                |s| s.settings.h264_profile = s.settings.h264_profile.next(),
                                cx,
                            ))
                            .child(setting_button(
                                "Toggle low latency",
                                "low-latency",
                                shared.clone(),
                                commands.clone(),
                                |s| {
                                    s.settings.low_latency_rate_control =
                                        !s.settings.low_latency_rate_control
                                },
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(setting_button(
                                "Toggle VSync",
                                "vsync",
                                shared.clone(),
                                commands.clone(),
                                |s| s.settings.vsync = !s.settings.vsync,
                                cx,
                            ))
                            .child(setting_button(
                                "Save custom preset",
                                "save-preset",
                                shared,
                                commands.clone(),
                                |s| {
                                    let custom = Preset {
                                        name: format!("Custom {}", s.presets.len() - 2),
                                        settings: s.settings.clone(),
                                    };
                                    s.presets.push(custom);
                                    match save_custom_presets(&s.presets[3..]) {
                                        Ok(()) => s.message = "Custom preset saved locally".into(),
                                        Err(error) => {
                                            s.message = format!("Could not save preset: {error}")
                                        }
                                    }
                                },
                                cx,
                            )),
                    )
            } else {
                div()
            };
            let active_diagnostics = active_screen
                .as_ref()
                .map(|screen| screen.diagnostics.clone())
                .unwrap_or_else(|| state.diagnostics.clone());
            let stats_panel = if let Some(screen) = &active_screen {
                let id = screen.config.screen_id;
                div().id("viewer-stats-panel").child(
                    Button::new("viewer-inspect-stats")
                        .label("Open statistics")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.inspect_stats(id, cx))),
                )
            } else {
                div().id("viewer-stats-none")
            };
            let logs = if state.show_logs {
                div()
                    .id("viewer-logs-scroll")
                    .h(px(130.0))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(format!(
                        "File: {}",
                        active_diagnostics
                            .path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unavailable".into())
                    ))
                    .children(
                        active_diagnostics
                            .recent()
                            .into_iter()
                            .map(|line| div().child(line)),
                    )
            } else {
                div().id("viewer-logs-empty")
            };
            let logs_toggle = Button::new("logs-menu")
                .label(if state.show_logs {
                    "Logs ▴"
                } else {
                    "Logs ▾"
                })
                .outline()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.show_logs = !state.show_logs;
                    }
                    cx.notify();
                }));
            let list_toggle = Button::new("viewer-screen-view-toggle-list")
                .label("Selected stream details →")
                .primary()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.view = ScreenView::Details;
                    }
                    cx.notify();
                }));
            let details_toggle = Button::new("viewer-screen-view-toggle-details")
                .label("← Screens")
                .primary()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.view = ScreenView::Details;
                        state.show_codecs = false;
                        state.show_logs = false;
                    }
                    if let Ok(mut ws) = this.workspace.lock() {
                        ws.selected = None;
                    }
                    cx.notify();
                }));
            let screen_list_panel =
                if matches!(page, ControlsPage::Connect | ControlsPage::Connecting) {
                    div().id("viewer-screens-offline")
                } else if page == ControlsPage::Details {
                    div().id("viewer-screen-list-hidden")
                } else {
                    div()
                        .id("viewer-screen-list")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("SCREEN STREAMS"),
                        )
                        .child(div().flex().flex_col().gap_1().children(screen_cards))
                        .when(active_screen.is_some(), |panel| panel.child(list_toggle))
                };
            let stream_details_panel = if page == ControlsPage::Details {
                div()
                    .id("viewer-screen-details")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(details_toggle)
                    .child(
                        active_screen
                            .as_ref()
                            .map(|screen| {
                                format!(
                                    "Stream details · {} · {:?} · generation {}",
                                    screen.config.name,
                                    screen.config.codec,
                                    screen.config.generation
                                )
                            })
                            .unwrap_or_else(|| "No active screen stream".into()),
                    )
                    .when(
                        active_screen.as_ref().is_some_and(|s| s.terminal),
                        |panel| {
                            panel.child(
                                if active_screen
                                    .as_ref()
                                    .and_then(|s| s.frame.lock().ok().map(|f| f.cleanup_complete))
                                    .unwrap_or(false)
                                {
                                    "Cleanup complete · retry is available"
                                } else {
                                    "Closing video resources · waiting for worker exit…"
                                },
                            )
                        },
                    )
                    .child(div().flex().flex_wrap().gap_2().when(
                        active_screen.as_ref().is_some_and(|s| {
                            !s.removing
                                && (!s.terminal
                                    || s.frame.lock().map(|f| f.cleanup_complete).unwrap_or(false))
                        }),
                        |row| {
                            row.child(
                                Button::new("retry-stream")
                                    .label(if active_screen.as_ref().is_some_and(|s| s.terminal) {
                                        "Retry stream"
                                    } else {
                                        "Restart stream"
                                    })
                                    .primary()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(id) = this
                                            .state
                                            .lock()
                                            .ok()
                                            .and_then(|s| s.navigation.selected)
                                        {
                                            let _ =
                                                this.commands.try_send(Command::RetryScreen(id));
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("remove-stream")
                                    .label("Detach viewer")
                                    .danger()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(id) = this
                                            .state
                                            .lock()
                                            .ok()
                                            .and_then(|s| s.navigation.selected)
                                        {
                                            let _ =
                                                this.commands.try_send(Command::RemoveScreen(id));
                                        }
                                        cx.notify();
                                    })),
                            )
                        },
                    ))
                    .child("Source settings affect every viewer attached to this host screen.")
                    .child(logs_toggle)
                    .child(logs)
                    .when(
                        active_screen
                            .as_ref()
                            .is_some_and(|s| !s.terminal && !s.removing),
                        |panel| {
                            panel.child(
                                Button::new("show-display")
                                    .label("Show display window")
                                    .outline()
                                    .small()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let result = this
                                            .state
                                            .lock()
                                            .ok()
                                            .and_then(|state| {
                                                state
                                                    .screens
                                                    .iter()
                                                    .find(|screen| {
                                                        Some(screen.config.screen_id)
                                                            == state.navigation.selected
                                                    })
                                                    .map(|screen| screen.video_window.clone())
                                            })
                                            .map(|window| window.show());
                                        if let Some(Err(error)) = result {
                                            message(
                                                &this.state,
                                                format!("Cannot show display: {error:#}"),
                                            );
                                        }
                                        cx.notify();
                                    })),
                            )
                        },
                    )
                    .when(
                        active_screen.as_ref().is_some_and(|s| !s.removing),
                        |panel| {
                            panel
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if state.connected {
                                            "PRESETS · selected stream"
                                        } else {
                                            "PRESETS · next connection"
                                        }),
                                )
                                .child(div().flex().flex_wrap().gap_2().children(presets))
                                .child(
                                    Button::new("codec-menu")
                                        .label(if state.show_codecs {
                                            "Video settings ▴"
                                        } else {
                                            "Video settings ▾"
                                        })
                                        .outline()
                                        .small()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                state.show_codecs = !state.show_codecs;
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(codec_panel)
                                .child(stats_panel)
                        },
                    )
                    .when_some(frame_error, |panel, error| {
                        panel.child(div().text_sm().text_color(cx.theme().danger).child(error))
                    })
            } else if page == ControlsPage::Connect {
                div()
                    .id("viewer-next-connection-settings")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("PRESETS · next connection")
                    .child(div().flex().flex_wrap().gap_2().children(presets))
                    .child(
                        Button::new("connection-video-settings")
                            .label(if state.show_codecs {
                                "Video settings ▴"
                            } else {
                                "Video settings ▾"
                            })
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Ok(mut state) = this.state.lock() {
                                    state.show_codecs = !state.show_codecs;
                                }
                                cx.notify();
                            })),
                    )
                    .child(codec_panel)
                    .child("Source settings affect every viewer attached to this host screen.")
                    .child(logs_toggle)
                    .child(logs)
            } else {
                div().id("viewer-screen-details-hidden")
            };
            div().size_full().min_w_0().min_h_0()
                .on_mouse_down(MouseButton::Left, |event, window, _| resize_from_border(event, window))
                .bg(cx.theme().background).border_1().border_color(cx.theme().border)
                .text_color(cx.theme().foreground).flex().flex_col()
                .child(TitleBar::new().child("AirBridge · Viewer"))
                .child(div().px_3().py_2().bg(cx.theme().secondary).text_sm().child(state.message.clone()))
                .child(div().id("viewer-settings-scroll").flex_1().min_h_0().overflow_y_scroll().p_3().flex().flex_col().gap_3()
                    .when(page == ControlsPage::Connect, |panel| panel
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child("DISCOVERED HOSTS"))
                    .child(div().flex().flex_col().gap_1().children(hosts))
                    .child(Form::new().small()
                        .child(Field::new().label("Host screen ID").required(true).child(Input::new(&self.inputs[2]).small()))
                        .child(Field::new().label("Session account").child(Input::new(&self.inputs[3]).small()))
                        .child(Field::new().label("Password / screen PIN").required(true).child(Input::new(&self.inputs[1]).small().mask_toggle()))
                        .child(Field::new().label("Manual host address").description("Leave blank to use the selected discovered host").child(Input::new(&self.inputs[0]).small())))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(Button::new("connect").label("Connect").primary().small().on_click(cx.listener(move |_, _, _, cx| {
                                let state = connect_state.lock().unwrap().clone();
                                if state.connected || state.connecting { return; }
                                if state.target_screen == 0 && state.username.is_empty() { message(&connect_state, "Enter a valid host screen ID"); cx.notify(); return; }
                                if state.username.is_empty() && bridge_core::security::validate_pin(&state.pin).is_err() { message(&connect_state, "Enter the six-digit host PIN"); cx.notify(); return; }
                                let address = if !state.manual_address.is_empty() {
                                    state.manual_address.parse::<SocketAddr>().ok().or_else(|| state.manual_address.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, bridge_core::protocol::PORT)))
                                } else { state.hosts.get(state.selected_host).and_then(|host| host.addresses.first().copied()) };
                                if let Ok(mut s)=connect_state.lock(){s.auto_suspended=false;}
                                if let Some(address) = address { let _ = connect_commands.try_send(Command::Connect(address, state.pin, if state.override_host { Some(state.settings) } else { None })); }
                                else { message(&connect_state, "Select a discovered Mac or enter its IP address"); }
                                cx.notify();
                            })))
)
)
                    .child(div().flex().flex_wrap().gap_2().px_3().py_2()
                       .child(Button::new("workspace-back").label("← Sessions").primary().small().on_click(cx.listener(|this,_,_,cx|{
                          if let Ok(mut state)=this.state.lock(){state.show_codecs=false;state.show_logs=false;}
                          if let Ok(mut ws)=this.workspace.lock(){ws.selected=None;}cx.notify();})))
                       .child(Button::new("workspace-remove").label("Remove local session").danger().small().on_click(cx.listener(|this,_,_,cx|{let id=this.workspace.lock().ok().and_then(|ws|ws.selected);if let Some(id)=id{remove_local_screen(&this.workspace,id);}cx.notify();}))))
                    .when(state.connected || state.connecting, |panel| panel.child(
                        Button::new("disconnect").label(if state.connecting { "Cancel connection" } else { "Disconnect session" }).danger().small().on_click(cx.listener(|this, _, _, _| { if let Ok(mut s)=this.state.lock(){s.auto_suspended=true;}let _ = this.commands.try_send(Command::Disconnect); }))))
                    .when(!state.connected,|panel|panel.child(Button::new("offline-logs-toggle").label(if state.show_logs {"Hide logs"}else{"Show logs"}).outline().small().on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.show_logs= !s.show_logs;}cx.notify();}))))
                    .when(!state.connected && state.show_logs,|panel|panel.child(div().id("offline-logs").h(px(150.)).overflow_y_scroll().p_2().children(state.diagnostics.recent().into_iter().map(|line|div().text_sm().child(line)))))
                    .child(div().flex().flex_col().gap_2()
                        .child(Button::new("remember-session").label(if state.remember{"Remember credentials: on"}else{"Remember credentials: off"}).outline().small().on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.remember=!s.remember;s.auto_connect=s.remember;s.auto_suspended=false;if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}else if s.remember{s.remembered=saved_viewer_session(&s);}}cx.notify();})))
                        .child(Button::new("auto-connect-session").label(if state.auto_connect{"Auto-connect: on"}else{"Auto-connect: off"}).outline().small().disabled(!state.remember).on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.auto_connect=!s.auto_connect;s.auto_suspended=false;if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}}cx.notify();})))
                        .child(Button::new("viewer-keep-awake").label(if state.preferences.keep_awake{"Keep viewer awake: on"}else{"Keep viewer awake: off"}).outline().small().on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.preferences.keep_awake=!s.preferences.keep_awake;}cx.notify();})))
                        .child(state.power_notice.clone())
                        .child(Button::new("forget-session").label("Forget saved login").danger().small().disabled(state.contract.is_none()).on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){if let Some(contract)=&s.contract{match bridge_core::sessions::forget_viewer(&contract.session_id){Ok(())=>{s.remembered=None;s.authenticated_key=None;s.remember=false;s.auto_connect=false;s.auto_suspended=true;s.message="Saved credentials forgotten".into();},Err(e)=>s.message=format!("Cannot forget login: {e}")}}}cx.notify();}))))
                    .child(screen_list_panel)
                    .child(stream_details_panel))
                    .into_any_element()
        }
    }

    impl ViewerView {
        fn new(
            workspace: Arc<Mutex<Workspace>>,
            state: Arc<Mutex<State>>,
            commands: mpsc::Sender<Command>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let inputs = [
                cx.new(|cx| InputState::new(window, cx).placeholder("192.168.1.10 or IP:port")),
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("Password or screen PIN")
                        .masked(true)
                        .validate(|value, _| value.len() <= 256)
                }),
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("1")
                        .validate(|value, _| {
                            value.len() <= 10 && value.bytes().all(|b| b.is_ascii_digit())
                        })
                }),
                cx.new(|cx| InputState::new(window, cx).placeholder("Account (blank for PIN)")),
            ];
            let subscriptions = inputs
                .iter()
                .enumerate()
                .map(|(index, input)| {
                    cx.subscribe_in(input, window, move |this, input, event, _, cx| {
                        if matches!(event, InputEvent::Change) {
                            let value = input.read(cx).value().to_string();
                            this.input_values[index] = value.clone();
                            if let Ok(mut state) = this.state.lock() {
                                match index {
                                    0 => state.manual_address = value,
                                    1 if state.pin != value => {
                                        bridge_core::sessions::clear_secret(&mut state.pin);
                                        state.pin = value;
                                        state.remembered = None;
                                        state.authenticated_key = None;
                                        state.auto_suspended = true;
                                    }
                                    3 if state.username != value => {
                                        state.username = value;
                                        state.remembered = None;
                                        state.contract = None;
                                        state.authenticated_key = None;
                                    }
                                    2 => {
                                        state.target_screen = value.parse::<u32>().unwrap_or(0);
                                        state.target_input = value;
                                    }
                                    _ => {}
                                }
                            }
                            cx.notify();
                        }
                    })
                })
                .collect();
            let mut this = Self {
                workspace,
                catalog: state.clone(),
                state,
                commands,
                inputs,
                input_owner: None,
                input_values: Default::default(),
                _input_subscriptions: subscriptions,
            };
            this.refresh(window, cx);
            this
        }

        fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if let Ok(mut ws) = self.workspace.lock() {
                ws.retiring.retain(|e| {
                    let finished = e
                        .thread
                        .lock()
                        .map(|t| t.as_ref().is_none_or(|t| t.is_finished()))
                        .unwrap_or(true);
                    if finished {
                        if let Some(t) = e.thread.lock().ok().and_then(|mut t| t.take()) {
                            let _ = t.join();
                        }
                    }
                    !finished
                });
            }

            let entry = self.workspace.lock().ok().and_then(|ws| {
                ws.entries
                    .iter()
                    .find(|e| Some(e.id) == ws.selected)
                    .cloned()
            });
            if let Some(entry) = entry {
                let owner_changed = self.input_owner != Some(entry.id);
                self.state = entry.state;
                self.commands = entry.commands;
                self.input_owner = Some(entry.id);
                let values = self
                    .state
                    .lock()
                    .map(|s| {
                        [
                            s.manual_address.clone(),
                            s.pin.clone(),
                            s.target_input.clone(),
                            s.username.clone(),
                        ]
                    })
                    .unwrap_or_default();
                for (index, value) in values.into_iter().enumerate() {
                    if owner_changed || self.input_values[index] != value {
                        self.inputs[index]
                            .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
                        self.input_values[index] = value;
                    }
                }
            } else {
                self.input_owner = None;
            }
            if let Ok(mut state) = self.state.lock() {
                for screen in &mut state.screens {
                    if screen.stats_view.open {
                        screen.stats_view.refresh(&screen.telemetry);
                    }
                }
            }
            if let Ok(mut s) = self.state.lock() {
                if !s.connected && s.contract.is_some() {
                    if let Some(saved) = &s.remembered {
                        if bridge_core::sessions::preferences_snapshot(&s.preferences).ok()
                            != bridge_core::sessions::preferences_snapshot(&saved.preferences).ok()
                        {
                            if let Err(e) = save_viewer_session(&s) {
                                s.message = format!("Session save failed: {e}");
                            } else {
                                let preferences = s.preferences.clone();
                                if let Some(saved) = &mut s.remembered {
                                    saved.preferences = preferences;
                                }
                            }
                        }
                    }
                }
            }
            cx.notify();
        }
    }

    fn open_controls(
        cx: &mut App,
        workspace: Arc<Mutex<Workspace>>,
        state: Arc<Mutex<State>>,
        commands: mpsc::Sender<Command>,
    ) -> Result<AnyWindowHandle> {
        let widget_size = size(px(400.0), px(540.0));
        let bounds = cx
            .primary_display()
            .map(|display| {
                let area = display.bounds();
                bounds(
                    point(
                        area.origin.x + area.size.width - widget_size.width - px(20.0),
                        area.origin.y + px(70.0),
                    ),
                    widget_size,
                )
            })
            .unwrap_or_else(|| Bounds::centered(None, widget_size, cx));
        let (handle, _) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(330.0), px(300.0))),
                app_id: Some("airbridge-viewer".into()),
                ..TitleBar::window_options()
            },
            cx,
            |window, cx| {
                window.set_window_title("AirBridge Controls");
                let view = cx.new(|cx| ViewerView::new(workspace, state, commands, window, cx));
                let controls_id = window.window_handle().window_id();
                cx.on_window_closed(move |cx, closed_id| {
                    if closed_id == controls_id {
                        cx.quit();
                    }
                })
                .detach();
                let window_handle = window.window_handle();
                let weak = view.downgrade();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        if window_handle
                            .update(cx, |_, window, cx| {
                                weak.update(cx, |this, cx| this.refresh(window, cx))
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
                view
            },
        )?;
        Ok(handle)
    }

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
                        attempt+=1;next_auto=std::time::Instant::now()+bridge_core::sessions::reconnect_delay(attempt);Command::Connect(address,String::new(),None)
                    }
                }};
                if let Command::Connect(address, pin, settings) = command {
                    if let Ok(mut state) = shared.lock() { state.connecting = true; state.terminal_reason=None; }
                    let began=std::time::Instant::now();
                    let result = connect(address, pin, settings, shared.clone(), &mut commands, shutdown.clone()).await;
                    if began.elapsed()>Duration::from_secs(30){attempt=0;}
                    next_auto=std::time::Instant::now()+bridge_core::sessions::reconnect_delay(attempt);
                    clear_client_session(&shared);
                    if let Err(error) = result {let cause=shared.lock().ok().and_then(|s|s.terminal_reason.clone());message(&shared,cause.unwrap_or_else(||format!("Connection failed: {error:#}")));}
                    if *shutdown.borrow() { break; }
                    reconnect=shared.lock().ok().and_then(|mut state|{if !state.retry_requested{return None;}state.retry_requested=false;Some(Command::Connect(state.last_endpoint?,state.pin.clone(),None))});
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
                state.adding = false;
                state.navigation.clear();
                state.show_logs = false;
                state.show_codecs = false;
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
            requested_revision: config.revision,
            desired_settings: config.settings.clone(),
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

    async fn install_client_screen(
        shared: &Arc<Mutex<State>>,
        config: ScreenConfig,
        select: bool,
    ) -> Result<()> {
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
                replacement.requested_revision = screen.requested_revision;
                replacement.desired_settings = screen.desired_settings;
                replacement.stats_view = screen.stats_view;
                replacement
            }
            None => new_client_screen(config.clone(), shared).await,
        };
        screen.cancel.send_replace(true);
        screen.cancel = watch::channel(false).0;
        screen.terminal = false;
        screen.cleanup_reported = false;
        if config.revision >= screen.requested_revision {
            screen.requested_revision = config.revision;
            screen.desired_settings = config.settings.clone();
        }
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
        if let Ok(mut frame) = screen.frame.lock() {
            frame.width = config.settings.width;
            frame.height = config.settings.height;
            frame.frames = 0;
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
            if select || state.navigation.selected.is_none() {
                state.navigation.open(config.screen_id);
                state.show_logs = false;
                state.show_codecs = false;
            }
            if state.navigation.selected == Some(config.screen_id) {
                state.settings = screen.desired_settings.clone();
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn install_during_session(
        shared: &Arc<Mutex<State>>,
        config: ScreenConfig,
        select: bool,
        connection: &quinn::Connection,
        commands: &mut mpsc::Receiver<Command>,
        queued: &mut VecDeque<Command>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<bool> {
        if *shutdown.borrow() {
            return Ok(false);
        }
        let install = install_client_screen(shared, config, select);
        tokio::pin!(install);
        loop {
            tokio::select! {
                _ = shutdown.changed() => return Ok(false),
                reason = connection.closed() => bail!("Host disconnected while opening the display: {reason}"),
                result = &mut install => return result.map(|()| true),
                command = commands.recv() => match command {
                    Some(Command::Disconnect) | None => return Ok(false),
                    Some(command) => {
                        if let Command::ChangeScreenSettings(id,_,_) = &command {queued.retain(|c|!matches!(c,Command::ChangeScreenSettings(other,_,_) if other==id));}
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
            let fallback = state.screens.first().map(|screen| screen.config.screen_id);
            if state.navigation.selected == Some(screen_id) {
                state.show_logs = false;
                state.show_codecs = false;
            }
            state.navigation.remove(screen_id, fallback);
            if let Some(screen) = state
                .screens
                .iter()
                .find(|screen| Some(screen.config.screen_id) == state.navigation.selected)
            {
                state.settings = screen.desired_settings.clone();
            }
            state.message = format!("Screen {screen_id} removed");
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
        pending: &mut HashMap<RequestId, PendingAction>,
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
            Some(screen)
        });
        let Some(screen) = screen else {
            return Ok(());
        };
        screen.cancel.send_replace(true);
        let _ = screen.video_window.close();
        let _ = pending;
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
        settings: Option<VideoSettings>,
        shared: Arc<Mutex<State>>,
        commands: &mut mpsc::Receiver<Command>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let pin = bridge_core::sessions::Secret(pin);
        let settings = settings.map(|mut settings| {
            settings.audio = false;
            settings
        });
        if let Some(settings) = &settings {
            settings.validate()?;
        }
        let (login, remembered) = shared
            .lock()
            .map(|s| {
                let login = (!s.username.is_empty()).then(|| bridge_core::security::SessionLogin {
                    username: s.username.clone(),
                    session_id: s.contract.as_ref().map(|c| c.session_id.clone()),
                    host_id: s
                        .contract
                        .as_ref()
                        .map(|c| c.host_id.clone())
                        .or_else(|| s.hosts.get(s.selected_host).map(|h| h.host_id.clone())),
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
                initial_settings: settings,
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
        for (index, screen) in screens.into_iter().enumerate() {
            if !install_during_session(
                &shared,
                screen,
                index == 0,
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
            state.override_host = false;
            if let Some(first) = state.screens.first() {
                state.settings = first.config.settings.clone();
            }
            state.bytes = 0;
            state.received_frames = 0;
        }
        message(&shared, "Connected · restoring session screens");
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
        let mut pending_requests = HashMap::<RequestId, PendingAction>::new();
        let power_state = shared.clone();
        let power = bridge_core::power::PowerController::new(move |notice| {
            if let Ok(mut s) = power_state.lock() {
                s.power_notice = notice;
            }
        });
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
                        power.set_active(shared.lock().map(|s|s.preferences.keep_awake).unwrap_or(false)&&screens.iter().any(|s|!s.terminal&&s.frame.lock().map(|f|f.frames>0&&!f.closed).unwrap_or(false)));
                        if let Ok(mut s)=shared.lock(){for screen in &screens{if let Ok(frame)=screen.frame.lock(){if frame.window.width>0&&frame.window.height>0{if let Some(saved)=s.preferences.screens.iter_mut().find(|p|p.screen_id==screen.config.screen_id){saved.window=frame.window.clone();}else if s.preferences.screens.len()<MAX_SCREENS{s.preferences.screens.push(bridge_core::sessions::ViewerScreen{screen_id:screen.config.screen_id,window:frame.window.clone()});}}}}}
                        let preferences=shared.lock().ok().filter(|s|s.contract.is_some()).map(|s|s.preferences.clone());
                        if let Some(preferences)=preferences{let snapshot=bridge_core::sessions::preferences_snapshot(&preferences)?;if snapshot!=last_preferences&&last_preferences_at.elapsed()>=Duration::from_secs(1){send_control(&mut control_send,&ClientMessage::SessionPreferences(preferences)).await?;if let Ok(mut s)=shared.lock(){if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}}last_preferences=snapshot;last_preferences_at=std::time::Instant::now();}}

                        counters.retain(|id,_|screens.iter().any(|s|s.config.screen_id==*id));
                        let stats_now=now_us(); let collect=stats_now.saturating_sub(last_stats)>=1_000_000;
                        for screen in screens {
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
                                request_remove_screen(&shared, &mut control_send, screen.config.screen_id, &mut pending_requests).await?;
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
                                request_remove_screen(&shared, &mut control_send, screen_id, &mut pending_requests).await?;
                            }
                            Some(Command::RetryScreen(id)) => {
                                if !shared.lock().map(|s|s.screens.iter().any(|screen|screen.config.screen_id==id && (!screen.terminal || screen.frame.lock().map(|f|f.cleanup_complete).unwrap_or(false)))).unwrap_or(false){continue;}
                                if let Ok(mut state)=shared.lock(){state.retry_requested=true;state.message="Reconnecting this screen…".into();}
                                break;
                            }
                            Some(Command::ChangeScreenSettings(screen_id, revision, mut settings)) => {
                                let exists = shared.lock().map(|state| state.screens.iter().any(|screen| screen.config.screen_id == screen_id && !screen.removing)).unwrap_or(false);
                                if !exists { continue; }
                                settings.audio = false;
                                let request_id = next_request_id(&shared);
                                if pending_requests.len()>=64{if let Some(old)=pending_requests.keys().copied().min(){pending_requests.remove(&old);}}
                                pending_requests.insert(request_id, PendingAction::Settings(screen_id, revision));
                                send_control(&mut control_send, &ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest { request_id, screen_id, revision, settings })).await?;
                                if let Ok(mut state) = shared.lock() {
                                    if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id) {
                                        screen.status = "Applying settings · restarting the stream…".into();
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
                                if !install_during_session(&shared,screen,false,&connection,commands,&mut queued_commands,&mut shutdown).await?{break;}
                            },
                            HostMessage::SessionUpdated(contract)=>{contract.validate()?;if let Ok(mut s)=shared.lock(){if s.contract.as_ref().is_none_or(|c|c.session_id!=contract.session_id||c.host_id!=contract.host_id||contract.revision<c.revision){bail!("Session update identity/revision mismatch");}s.preferences.screens.retain(|p|contract.screens.iter().any(|screen|screen.screen_id==p.screen_id));s.contract=Some(contract);if let Err(e)=save_viewer_session(&s){s.message=format!("Session save failed: {e}");}}},
                            HostMessage::PersistentSessionAccepted{..}=>bail!("Unexpected duplicate persistent session acceptance"),
                            HostMessage::ScreenSettingsApplied { request_id, screen } => {
                                if !shared.lock().map(|s|s.screens.iter().any(|current|current.config.screen_id==screen.screen_id)).unwrap_or(false){bail!("Settings belong to an unattached screen");}
                                pending_requests.retain(|_,action|!matches!(action,PendingAction::Settings(id,revision) if *id==screen.screen_id&&*revision<=screen.revision));
                                if let Some(id) = request_id { pending_requests.remove(&id); }
                                match install_during_session(&shared, screen.clone(), false, &connection, commands, &mut queued_commands, &mut shutdown).await {
                                    Ok(false) => break,
                                    Ok(true) => message(&shared, format!("{} settings applied · stream generation {}", screen.name, screen.generation)),
                                    Err(error) => message(&shared, format!("Could not apply {} settings: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenRemoved { request_id, screen_id } => {
                                if let Some(id) = request_id { pending_requests.remove(&id); }
                                pending_requests.retain(|_, action| !matches!(action, PendingAction::Settings(id, _) if *id == screen_id));
                                if let Ok(mut state)=shared.lock(){state.terminal_reason=Some(format!("Host screen {screen_id} was removed"));}
                                remove_client_screen(&shared, screen_id).await;
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
                                let pending = request_id.and_then(|id| pending_requests.remove(&id));
                                if let Ok(mut state) = shared.lock() {
                                    match pending {

                                        Some(PendingAction::Settings(id,revision)) => {
                                            if let Some(screen)=state.screens.iter_mut().find(|s|s.config.screen_id==id && s.requested_revision==revision){screen.desired_settings=screen.config.settings.clone();}
                                            if state.navigation.selected==Some(id){if let Some(settings)=state.screens.iter().find(|s|s.config.screen_id==id).map(|s|s.desired_settings.clone()){state.settings=settings;}}
                                        }
                                        None => {},
                                    }
                                }
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
    fn initial_state(presets: Vec<Preset>) -> State {
        State {
            clock: ClockEstimate::default(),
            hosts: Vec::new(),
            selected_host: 0,
            target_screen: 1,
            target_input: "1".into(),
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
            manual_address: String::new(),
            settings: presets[1].settings.clone(),
            override_host: false,
            screens: Vec::new(),
            removed_screens: HashSet::new(),
            navigation: ScreenNavigation::default(),
            next_request_id: 1,
            connected: false,
            connecting: false,
            adding: false,
            presets,
            selected_preset: 1,
            message: "Discovering Macs…".into(),
            terminal_reason: None,
            retry_requested: false,
            last_endpoint: None,
            show_codecs: false,
            show_logs: false,
            diagnostics: Arc::new(Diagnostics::new("viewer")),
            bytes: 0,
            received_frames: 0,
        }
    }

    pub fn main() {
        let mut presets = built_in_presets();
        if let Ok(custom) = load_custom_presets() {
            presets.extend(custom);
        }
        let state = Arc::new(Mutex::new(initial_state(presets)));
        let workspace = Arc::new(Mutex::new(Workspace::default()));
        match bridge_core::sessions::ViewerStore::load() {
            Ok(store) => {
                if let Ok(mut s) = state.lock() {
                    s.stable_viewer = store.viewer_id;
                }
                for saved in store.sessions {
                    add_local_session(&workspace, &state, Some(saved));
                }
                if let Ok(mut ws) = workspace.lock() {
                    ws.selected = None;
                }
            }
            Err(e) => message(&state, format!("Saved logins unavailable: {e}")),
        }

        let (send, receive) = mpsc::channel(64);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        drop(receive);
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
                let controls =
                    open_controls(cx, app_workspace.clone(), state.clone(), send.clone())
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
    #[cfg(test)]
    mod tests {
        use super::*;
        fn state() -> Arc<Mutex<State>> {
            Arc::new(Mutex::new(initial_state(built_in_presets())))
        }
        fn screen(id: ScreenId, generation: u64) -> ClientScreen {
            let frame = Arc::new(Mutex::new(VideoFrame::default()));
            ClientScreen {
                config: ScreenConfig {
                    screen_id: id,
                    name: format!("Screen {id}"),
                    revision: 2,
                    generation,
                    settings: built_in_presets()[1].settings.clone(),
                    instrumented: false,
                    codec: CodecOption {
                        codec: Codec::H264,
                        acceleration: Acceleration::Software,
                    },
                },
                requested_revision: 2,
                desired_settings: built_in_presets()[1].settings.clone(),
                status: "Streaming".into(),
                terminal: false,
                cleanup_reported: false,
                removing: false,
                cancel: watch::channel(false).0,
                video_window: VideoWindow::unavailable("Test backend".into(), frame.clone()),
                frame,
                diagnostics: Arc::new(Diagnostics::new("viewer-lifecycle-test")),
                bytes: 0,
                received_frames: 0,
                host_frames_sent: 0,
                host_frames_dropped: 0,
                bitrate_kbps: 0,
                telemetry: Telemetry::new(),
                stats_view: StatsView::default(),
            }
        }
        #[test]
        fn late_diagnostic_batches_cannot_update_replacement_generation() {
            let shared = state();
            let mut s = screen(1, 7);
            s.config.instrumented = true;
            let telemetry = s.telemetry.clone();
            shared.lock().unwrap().screens.push(s);
            ingest_diagnostic_batch(
                &shared,
                bridge_core::telemetry::DiagnosticBatch {
                    screen_id: 1,
                    generation: 6,
                    events: vec![],
                    stats: None,
                    lost_events: 100,
                },
            );
            assert_eq!(telemetry.snapshot().peer_lost_events, 0);
            ingest_diagnostic_batch(
                &shared,
                bridge_core::telemetry::DiagnosticBatch {
                    screen_id: 1,
                    generation: 7,
                    events: vec![],
                    stats: None,
                    lost_events: 3,
                },
            );
            assert_eq!(telemetry.snapshot().peer_lost_events, 3);
        }

        #[test]
        fn visibility_tracks_connection_list_details_and_missing_selection() {
            let shared = state();
            let mut state = shared.lock().unwrap();
            assert_eq!(state.page(), ControlsPage::Connect);
            state.connecting = true;
            assert_eq!(state.page(), ControlsPage::Connecting);
            state.connected = true;
            state.connecting = false;
            assert_eq!(state.page(), ControlsPage::List);
            state.screens.push(screen(1, 3));
            state.navigation.open(1);
            assert_eq!(state.page(), ControlsPage::Details);
            state.navigation.back();
            assert_eq!(state.page(), ControlsPage::List);
            state.navigation.open(99);
            assert_eq!(state.page(), ControlsPage::List);
            state.navigation.open(1);
            state.connecting = true;
            assert_eq!(state.page(), ControlsPage::Connecting);
            state.connecting = false;
            state.connected = false;
            assert_eq!(state.page(), ControlsPage::Connect);
        }
        #[test]
        fn old_generation_events_cannot_close_or_change_replacement() {
            let shared = state();
            shared.lock().unwrap().screens.push(screen(1, 5));
            end_client_lane(&shared, 1, 4, "Old lane closed".into());
            update_client_screen_status(&shared, 1, 4, "Old lane ready".into());
            assert!(!shared.lock().unwrap().screens[0].terminal);
            end_client_lane(&shared, 1, 5, "Capture failed".into());
            update_client_screen_status(&shared, 1, 5, "Late ready".into());
            let state = shared.lock().unwrap();
            assert!(state.screens[0].terminal);
            assert!(*state.screens[0].cancel.borrow());
            assert_eq!(state.screens[0].status, "Capture failed");
        }

        #[gpui_kit::test]
        fn kit_inputs_edit_and_keep_each_screens_draft(cx: &mut gpui_kit::TestAppContext) {
            use gpui_kit::test::TestWindowExt;
            let first = Arc::new(Mutex::new(initial_state(built_in_presets())));
            let second = Arc::new(Mutex::new(initial_state(built_in_presets())));
            second.lock().unwrap().pin = "654321".into();
            let (commands, _receiver) = mpsc::channel(16);
            let entry = |id, state: Arc<Mutex<State>>| LocalScreen {
                id,
                state,
                commands: commands.clone(),
                shutdown: watch::channel(false).0,
                thread: Arc::new(Mutex::new(None)),
            };
            let workspace = Arc::new(Mutex::new(Workspace {
                entries: vec![entry(1, first.clone()), entry(2, second.clone())],
                selected: Some(1),
                ..Default::default()
            }));
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        ViewerView::new(workspace.clone(), first.clone(), commands, window, cx)
                    })
                })
                .unwrap()
            });
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    let input = view.read(cx).inputs[1].clone();
                    input.update(cx, |input, cx| input.focus(window, cx));
                    window.input("123456", cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(first.lock().unwrap().pin, "123456");
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("112233".into()));
            handle
                .update(cx, |_, window, cx| {
                    window.press(
                        if cfg!(target_os = "macos") {
                            "cmd-a"
                        } else {
                            "ctrl-a"
                        },
                        cx,
                    );
                    window.press(
                        if cfg!(target_os = "macos") {
                            "cmd-v"
                        } else {
                            "ctrl-v"
                        },
                        cx,
                    );
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(first.lock().unwrap().pin, "112233");
            handle
                .update(cx, |_, window, cx| window.input("a", cx))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                first.lock().unwrap().pin,
                "112233a",
                "The password input accepts non-digit credentials; PINs are checked on connect"
            );
            workspace.lock().unwrap().selected = Some(2);
            handle
                .update(cx, |_, window, cx| {
                    view.update(cx, |this, cx| this.refresh(window, cx))
                })
                .unwrap();
            cx.read(|cx| assert_eq!(view.read(cx).inputs[1].read(cx).value().as_ref(), "654321"));
            workspace.lock().unwrap().selected = Some(1);
            handle
                .update(cx, |_, window, cx| {
                    view.update(cx, |this, cx| this.refresh(window, cx))
                })
                .unwrap();
            cx.read(|cx| assert_eq!(view.read(cx).inputs[1].read(cx).value().as_ref(), "112233a"));
            handle
                .update(cx, |_, window, cx| {
                    let input = view.read(cx).inputs[0].clone();
                    input.update(cx, |input, cx| input.focus(window, cx));
                    window.input("192.168.1.5", cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(first.lock().unwrap().manual_address, "192.168.1.5");
            handle
                .update(cx, |_, window, cx| {
                    window.press("left", cx);
                    view.update(cx, |this, cx| this.refresh(window, cx));
                    window.input("0", cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                first.lock().unwrap().manual_address,
                "192.168.1.05",
                "periodic refresh must not reset the caret"
            );
            assert!(second.lock().unwrap().manual_address.is_empty());
        }
        fn remembered_state() -> State {
            let mut s = initial_state(built_in_presets());
            let contract = bridge_core::sessions::SessionContract {
                version: bridge_core::sessions::STORE_VERSION,
                session_id: bridge_core::sessions::new_id().unwrap(),
                host_id: bridge_core::sessions::new_id().unwrap(),
                username: "alice".into(),
                revision: 1,
                keep_awake: false,
                screens: vec![],
            };
            s.username = "alice".into();
            s.remember = true;
            s.auto_connect = true;
            s.contract = Some(contract.clone());
            s.remembered = Some(bridge_core::sessions::SavedViewerSession {
                contract,
                viewer_id: s.stable_viewer.clone(),
                key: bridge_core::sessions::Secret("11".repeat(32)),
                preferences: Default::default(),
                auto_connect: true,
            });
            s
        }
        #[test]
        fn auto_connection_matches_identity_and_respects_manual_disconnect() {
            let mut s = remembered_state();
            let host_id = s.contract.as_ref().unwrap().host_id.clone();
            s.hosts.push(DiscoveredHost {
                name: "unrelated".into(),
                host_id: "different-host".into(),
                screen_id: 1,
                addresses: vec!["127.0.0.1:4444".parse().unwrap()],
            });
            assert!(s.auto_endpoint().is_none());
            let address = "127.0.0.1:44990".parse().unwrap();
            s.hosts.push(DiscoveredHost {
                name: "known-host".into(),
                host_id,
                screen_id: 0,
                addresses: vec![address],
            });
            assert_eq!(s.auto_endpoint(), Some(address));
            s.auto_suspended = true;
            assert!(s.auto_endpoint().is_none());
            s.auto_suspended = false;
            s.remember = false;
            assert!(s.auto_endpoint().is_none());
        }
        #[gpui_kit::test]
        fn input_refresh_preserves_saved_session_identity_and_secret(
            cx: &mut gpui_kit::TestAppContext,
        ) {
            use gpui_kit::test::TestWindowExt;
            let state = Arc::new(Mutex::new(remembered_state()));
            let (commands, _) = mpsc::channel(16);
            let entry = LocalScreen {
                id: 1,
                state: state.clone(),
                commands: commands.clone(),
                shutdown: watch::channel(false).0,
                thread: Arc::new(Mutex::new(None)),
            };
            let workspace = Arc::new(Mutex::new(Workspace {
                entries: vec![entry],
                selected: Some(1),
                ..Default::default()
            }));
            cx.update(gpui_kit::init);
            let (handle, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| ViewerView::new(workspace, state.clone(), commands, window, cx))
                })
                .unwrap()
            });
            cx.run_until_parked();
            assert!(state.lock().unwrap().remembered.is_some());
            assert!(state.lock().unwrap().contract.is_some());
            handle
                .update(cx, |_, window, cx| {
                    view.update(cx, |this, cx| this.refresh(window, cx));
                    let input = view.read(cx).inputs[1].clone();
                    input.update(cx, |input, cx| input.focus(window, cx));
                    window.input("new-password", cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert!(state.lock().unwrap().remembered.is_none());
            assert!(state.lock().unwrap().contract.is_some());
            assert_eq!(state.lock().unwrap().pin, "new-password");
        }
        #[tokio::test]
        async fn completed_media_task_is_not_polled_again_during_cleanup() {
            let mut task = Some(tokio::spawn(async {}));
            tokio::select! {
                result = async { task.as_mut().unwrap().await } => {
                    assert!(result.is_ok());
                    task.take();
                }
            }
            stop_lane_manager(task).await;
        }

        #[tokio::test]
        async fn panicked_media_task_is_not_polled_again_during_cleanup() {
            let mut task = Some(tokio::spawn(async {
                panic!("fixture media task failure");
            }));
            assert!(task.as_mut().unwrap().await.unwrap_err().is_panic());
            task.take();
            stop_lane_manager(task).await;
        }

        #[tokio::test]
        async fn active_media_task_is_cancelled_and_joined_during_cleanup() {
            let (ready, began) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _ = ready.send(());
                std::future::pending::<()>().await;
            });
            began.await.unwrap();
            stop_lane_manager(Some(task)).await;
        }

        #[tokio::test]
        async fn media_panic_is_scoped_and_unregisters_the_lane() {
            let key = (2, 7, MediaLane::Video);
            let active = Arc::new(Mutex::new(HashSet::from([key])));
            let (send, mut receive) = mpsc::channel(64);
            let registered = active.clone();
            let task = tokio::spawn(async move {
                let _registration = LaneRegistration {
                    key,
                    active: registered,
                    events: send,
                    shared: None,
                };
                panic!("simulated receiver failure");
            });
            assert!(task.await.unwrap_err().is_panic());
            assert!(active.lock().unwrap().is_empty());
            assert!(matches!(
                receive.recv().await,
                Some(LaneNotice::Failed(2, 7, MediaLane::Video, _))
            ));
        }
        #[tokio::test]
        async fn a_late_settings_ack_cannot_recreate_a_removed_screen() {
            let shared = state();
            let screen = screen(1, 3);
            let mut ack = screen.config.clone();
            shared.lock().unwrap().screens.push(screen);
            remove_client_screen(&shared, 1).await;
            ack.generation = 4;
            install_client_screen(&shared, ack, false).await.unwrap();
            assert!(shared.lock().unwrap().screens.is_empty());
            clear_client_session(&shared);
            assert!(shared.lock().unwrap().removed_screens.is_empty());
        }

        #[test]
        fn capture_error_after_media_fin_keeps_the_actual_failure_visible() {
            let shared = state();
            shared.lock().unwrap().screens.push(screen(1, 3));
            end_client_lane(&shared, 1, 3, "Video lane closed".into());
            end_client_lane(&shared, 1, 3, "Capture failed: permission denied".into());
            let diagnostics = shared.lock().unwrap().screens[0].diagnostics.clone();
            let count = diagnostics.recent().len();
            end_client_lane(&shared, 1, 3, "Capture failed: permission denied".into());
            end_client_lane(&shared, 1, 3, "Video lane closed".into());
            assert_eq!(diagnostics.recent().len(), count);
            assert_eq!(
                shared.lock().unwrap().screens[0].status,
                "Capture failed: permission denied"
            );
        }

        #[tokio::test]
        async fn rapid_edits_have_distinct_revisions_and_ack_keeps_newer_desired_settings() {
            let shared = state();
            let original = screen(1, 5);
            let mut ack = original.config.clone();
            {
                let mut state = shared.lock().unwrap();
                state.screens.push(original);
                state.navigation.open(1);
                state.settings.fps = 30;
                let first = persist_selected_settings(&mut state).unwrap();
                state.settings.fps = 45;
                let second = persist_selected_settings(&mut state).unwrap();
                assert_eq!((first.1, second.1), (3, 4));
                ack.revision = first.1;
                ack.generation = 6;
                ack.settings = first.2;
            }
            install_client_screen(&shared, ack, false).await.unwrap();
            let state = shared.lock().unwrap();
            assert_eq!(state.screens[0].config.settings.fps, 30);
            assert_eq!(state.screens[0].desired_settings.fps, 45);
            assert_eq!(state.settings.fps, 45);
            assert_eq!(state.screens[0].requested_revision, 4);
        }
        #[tokio::test]
        async fn removal_is_idempotent_and_last_screen_returns_to_empty_list() {
            let shared = state();
            {
                let mut state = shared.lock().unwrap();
                state.connected = true;
                state.screens.push(screen(1, 5));
                state.navigation.open(1);
                state.show_logs = true;
            }
            let (mut write, mut read) = tokio::io::duplex(4096);
            let mut pending = HashMap::new();
            request_remove_screen(&shared, &mut write, 1, &mut pending)
                .await
                .unwrap();
            request_remove_screen(&shared, &mut write, 1, &mut pending)
                .await
                .unwrap();
            assert_eq!(pending.len(), 0);
            let request: ClientMessage = receive_json(&mut read).await.unwrap();
            assert!(matches!(request, ClientMessage::Stop));
            {
                let state = shared.lock().unwrap();
                assert!(state.screens[0].removing);
                assert!(*state.screens[0].cancel.borrow());
            }
            remove_client_screen(&shared, 1).await;
            let state = shared.lock().unwrap();
            assert!(state.connected);
            assert!(state.screens.is_empty());
            assert_eq!(state.page(), ControlsPage::List);
            assert_eq!(state.navigation.selected, None);
            assert!(!state.show_logs);
        }
        #[tokio::test]
        async fn user_closed_display_is_not_resurrected_by_settings_ack() {
            let shared = state();
            let screen = screen(1, 3);
            let mut ack = screen.config.clone();
            screen.frame.lock().unwrap().user_closed = true;
            ack.generation = 4;
            ack.revision = 3;
            shared.lock().unwrap().screens.push(screen);
            install_client_screen(&shared, ack, false).await.unwrap();
            assert_eq!(shared.lock().unwrap().screens[0].config.generation, 3);
        }

        #[test]
        fn disconnect_cancels_all_screens_and_clears_session_ui() {
            let shared = state();
            let first = screen(1, 2);
            let second = screen(2, 7);
            let cancellations = [first.cancel.clone(), second.cancel.clone()];
            {
                let mut state = shared.lock().unwrap();
                state.connected = true;
                state.connecting = true;
                state.adding = true;
                state.screens = vec![first, second];
                state.navigation.open(2);
                state.show_logs = true;
                state.show_codecs = true;
            }
            clear_client_session(&shared);
            clear_client_session(&shared);
            assert!(cancellations.iter().all(|cancel| *cancel.borrow()));
            let state = shared.lock().unwrap();
            assert!(state.screens.is_empty());
            assert_eq!(state.page(), ControlsPage::Connect);
            assert!(!state.adding && !state.show_logs && !state.show_codecs);
            assert_eq!(state.navigation.selected, None);
        }
        #[test]
        fn app_shutdown_resets_network_state_without_opening_windows() {
            let shared = state();
            let screen = screen(1, 2);
            let cancel = screen.cancel.clone();
            {
                let mut state = shared.lock().unwrap();
                state.connected = true;
                state.screens.push(screen);
                state.navigation.open(1);
            }
            let (_commands, receive) = mpsc::channel(64);
            let (stop, stopped) = watch::channel(false);
            stop.send_replace(true);
            run_network(shared.clone(), receive, stopped).unwrap();
            assert!(*cancel.borrow());
            assert_eq!(shared.lock().unwrap().page(), ControlsPage::Connect);
        }

        #[tokio::test]
        async fn connection_can_be_cancelled_during_startup() {
            let shared = state();
            let (send, mut receive) = mpsc::channel(64);
            let (_shutdown, shutdown) = watch::channel(false);
            send.try_send(Command::Disconnect).unwrap();
            tokio::time::timeout(
                Duration::from_secs(1),
                connect(
                    "127.0.0.1:44991".parse().unwrap(),
                    "123456".into(),
                    None,
                    shared.clone(),
                    &mut receive,
                    shutdown,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(shared.lock().unwrap().screens.is_empty());
            assert!(shared.lock().unwrap().message.contains("cancelled"));
        }

        #[test]
        fn discovery_loss_preserves_selection_or_repairs_it() {
            let shared = state();
            let mut state = shared.lock().unwrap();
            for name in ["first", "second", "third"] {
                state.discovery_event(DiscoveryEvent::Found(DiscoveredHost {
                    name: name.into(),
                    addresses: vec![],
                    screen_id: 1,
                    host_id: "test-host".into(),
                }));
            }
            state.selected_host = 2;
            state.discovery_event(DiscoveryEvent::Lost("first".into()));
            assert_eq!(state.hosts[state.selected_host].name, "third");
            state.discovery_event(DiscoveryEvent::Lost("third".into()));
            assert_eq!(state.selected_host, 0);
            state.discovery_event(DiscoveryEvent::Lost("second".into()));
            assert!(state.hosts.is_empty());
            state.select_host("third");
            assert!(state.message.contains("no longer available"));
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    if linux::worker_entry() {
        return;
    }
    app::main();
}
