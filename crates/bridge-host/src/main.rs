#[cfg(target_os = "macos")]
mod mac;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("AirBridge host currently requires macOS");
}

#[cfg(target_os = "macos")]
mod app {
    use super::mac;
    use anyhow::{Result, bail};
    use bridge_core::{
        config::{
            Acceleration, COMMON_FRAME_RATES, COMMON_RESOLUTIONS, Codec, CodecField, CodecOption,
            CodecPreset, Control, HostSettings, OptionValue, VideoSettings, aspect_label,
            codec_availability, codec_schema,
        },
        diagnostics::Diagnostics,
        discovery::Advertiser,
        protocol::{
            ClientMessage, ConfigRevision, HostMessage, MAX_SCREENS, MediaLane, MediaState,
            MediaStreamHello, RequestId, ScreenConfig, ScreenId, StreamGeneration, VERSION,
            control_reader, receive_json, send_control, send_debug_media, send_json, send_media,
        },
        security::server_endpoint,
        sessions::{HostStore, SavedScreen, ViewerLockPolicy},
        telemetry::{Outcome, Stage, StatsView, StreamStats, Telemetry, TimingView, now_us},
    };
    use bridge_ui::{
        fields::{Fields, InputSpec},
        kit::{self, Choice, Pill},
    };
    use gpui_kit::component::{
        ActiveTheme, Disableable, IconName, Sizable, TitleBar,
        button::{Button, ButtonVariants},
        input::Input,
        menu::{DropdownMenu, PopupMenuItem},
        slider::Slider,
    };
    use gpui_kit::{
        AnyElement, App, Bounds, Context, FontWeight, SharedString, Window, WindowBounds,
        WindowDecorations, WindowOptions, bounds, div, point, prelude::*, px, size,
    };
    use std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    use tokio::sync::{mpsc, oneshot, watch};

    /// What the stream state machine reports for a screen; drives its status pill.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Phase {
        Waiting,
        Streaming,
        Error,
    }

    #[derive(Clone)]
    struct HostScreen {
        /// The OS display currently backing this slot. Never leaves the host.
        display_id: Option<u32>,
        display_position: Option<bridge_core::sessions::DisplayPosition>,
        pin: String,
        viewers: Vec<ViewerPeer>,
        stats_peer: Option<u64>,
        /// Stable slot identity; the only screen identifier viewers ever see.
        screen_id: ScreenId,
        /// Position among temporary screens (T-1, T-2, …); unused for session screens.
        temp_index: u32,
        name: String,
        revision: ConfigRevision,
        generation: StreamGeneration,
        settings: VideoSettings,
        /// Set while an edit made in the controls has not been applied yet.
        edited: Option<Instant>,
        codec: Option<CodecOption>,
        status: String,
        phase: Phase,
        removing: bool,
        frames: u64,
        dropped: u64,
        bytes: u64,
        diagnostics: Arc<Diagnostics>,
        telemetry: Arc<Telemetry>,
        stats_view: StatsView,
        viewer_stats: bool,
    }

    #[derive(Clone)]
    struct ViewerPeer {
        id: u64,
        name: String,
        status: String,
        telemetry: Arc<Telemetry>,
        codecs: Vec<CodecOption>,
    }

    impl HostScreen {
        fn new(screen_id: ScreenId, name: String, settings: VideoSettings, pin: String) -> Self {
            Self {
                display_id: None,
                display_position: None,
                pin,
                viewers: Vec::new(),
                stats_peer: None,
                screen_id,
                temp_index: 0,
                name,
                revision: 0,
                generation: 0,
                settings,
                edited: None,
                codec: None,
                status: "Prepared · waiting for a viewer".into(),
                phase: Phase::Waiting,
                removing: false,
                frames: 0,
                dropped: 0,
                bytes: 0,
                diagnostics: Arc::new(Diagnostics::new(&format!("host-slot-{screen_id}"))),
                telemetry: Telemetry::new(),
                stats_view: StatsView::default(),
                viewer_stats: false,
            }
        }
        fn instrumented(&self) -> bool {
            self.stats_view.open || self.viewer_stats
        }
        /// Decoders every attached viewer has; `None` while nobody is attached.
        fn viewer_codecs(&self) -> Option<Vec<CodecOption>> {
            let mut viewers = self.viewers.iter();
            let mut common = viewers.next()?.codecs.clone();
            for viewer in viewers {
                common.retain(|option| viewer.codecs.contains(option));
            }
            Some(common)
        }
    }

    enum HostUiCommand {
        AddScreen(ScreenId),
        ChangeSettings(ScreenId, VideoSettings),
        RemoveScreen(ScreenId),
        Diagnostics(ScreenId, bool),
        Burst(ScreenId),
        DisconnectPeer(ScreenId, u64),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Page {
        Home,
        Screen(ScreenId),
        NewSession,
    }

    #[derive(Clone)]
    struct State {
        message: String,
        /// True while `message` reports a problem the user should see.
        alerting: bool,
        screens: Vec<HostScreen>,
        page: Page,
        session_commands: Option<mpsc::Sender<HostUiCommand>>,
        diagnostics: Arc<Diagnostics>,
        sessions: HostStore,
        /// Session cards currently expanded on the home page.
        expanded: HashSet<String>,
        storage_snapshot: String,
        power_notice: String,
        settings: HostSettings,
        host_name: String,
    }

    impl State {
        fn alert(&mut self, message: impl Into<String>) {
            self.message = message.into();
            self.alerting = true;
            self.diagnostics.record("host", &self.message);
        }
        fn screen(&self, id: ScreenId) -> Option<&HostScreen> {
            self.screens.iter().find(|screen| screen.screen_id == id)
        }
        fn screen_mut(&mut self, id: ScreenId) -> Option<&mut HostScreen> {
            self.screens
                .iter_mut()
                .find(|screen| screen.screen_id == id)
        }
        fn send(&self, command: HostUiCommand) {
            if let Some(commands) = &self.session_commands {
                let _ = commands.try_send(command);
            }
        }
    }

    fn set_message(state: &Arc<Mutex<State>>, message: impl Into<String>) {
        let message = message.into();
        if let Ok(mut state) = state.lock() {
            state.diagnostics.record("host", &message);
            state.message = message;
            state.alerting = false;
        }
    }

    fn set_alert(state: &Arc<Mutex<State>>, message: impl Into<String>) {
        if let Ok(mut state) = state.lock() {
            state.alert(message);
        }
    }

    fn set_screen_message(state: &Arc<Mutex<State>>, screen_id: ScreenId, message: &str) {
        if let Ok(mut state) = state.lock() {
            if let Some(screen) = state.screen_mut(screen_id) {
                screen.status = message.to_owned();
                screen.diagnostics.record("stream", message);
            }
        }
    }

    fn set_screen_phase(
        state: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        phase: Phase,
        message: &str,
    ) {
        if let Ok(mut state) = state.lock() {
            if let Some(screen) = state.screen_mut(screen_id) {
                screen.phase = phase;
                screen.status = message.to_owned();
                screen.diagnostics.record("stream", message);
            }
        }
    }

    fn set_screen_generation_message(
        state: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        generation: StreamGeneration,
        message: &str,
    ) {
        if let Ok(mut state) = state.lock() {
            if let Some(screen) = state
                .screens
                .iter_mut()
                .find(|screen| screen.screen_id == screen_id && screen.generation == generation)
            {
                screen.status = message.to_owned();
                screen.diagnostics.record("stream", message);
            }
        }
    }

    /// "Slot A" for a remembered screen, "T-1" for a temporary one.
    fn slot_label(state: &State, screen: &HostScreen) -> String {
        state
            .sessions
            .owner(screen.screen_id)
            .and_then(|session| session.contract.slot_label(screen.screen_id))
            .unwrap_or_else(|| format!("T-{}", screen.temp_index))
    }

    /// Create a screen, remembered by `session` or temporary. Its ID is the
    /// lowest free slot, so re-creating a screen never grows the numbering.
    fn add_host_screen(
        state: &mut State,
        session: Option<&str>,
        settings: VideoSettings,
    ) -> Option<ScreenId> {
        if state.screens.len() >= MAX_SCREENS {
            state.alert(format!(
                "A maximum of {MAX_SCREENS} screens can be live at once"
            ));
            return None;
        }
        if session.is_some_and(|id| {
            state
                .sessions
                .session(id)
                .is_none_or(|s| s.contract.screens.len() >= MAX_SCREENS)
        }) {
            state.alert(format!("A session holds at most {MAX_SCREENS} screens"));
            return None;
        }
        let screen_id = state
            .sessions
            .free_screen_id(state.screens.iter().map(|screen| screen.screen_id));
        let pin = match random_pin() {
            Ok(pin) => pin,
            Err(error) => {
                state.alert(format!("Cannot generate screen PIN: {error}"));
                return None;
            }
        };
        let temp_index = (1..)
            .find(|index| {
                !state.screens.iter().any(|screen| {
                    screen.temp_index == *index && state.sessions.owner(screen.screen_id).is_none()
                })
            })
            .unwrap_or(1);
        let name = match session.and_then(|id| state.sessions.session(id)) {
            Some(owner) => format!("Screen {}", owner.contract.screens.len() + 1),
            None => format!("Temp {temp_index}"),
        };
        let mut screen = HostScreen::new(screen_id, name.clone(), settings.clone(), pin);
        screen.temp_index = temp_index;
        state.screens.push(screen);
        if let Some(owner) = session.and_then(|id| state.sessions.session_mut(id)) {
            owner.contract.screens.push(SavedScreen {
                position: None,
                screen_id,
                name: name.clone(),
                settings,
            });
            owner.contract.revision += 1;
        }
        state.send(HostUiCommand::AddScreen(screen_id));
        state.message = format!("{name} prepared");
        state.diagnostics.record("screen", &state.message);
        Some(screen_id)
    }

    /// Remember a temporary screen in a session. The slot keeps its ID and settings.
    fn move_to_session(state: &mut State, screen_id: ScreenId, session_id: &str) {
        let Some(screen) = state.screen(screen_id).cloned() else {
            return;
        };
        if state.sessions.owner(screen_id).is_some() {
            return;
        }
        let Some(session) = state.sessions.session_mut(session_id) else {
            return;
        };
        if session.contract.screens.len() >= MAX_SCREENS {
            state.alert(format!("A session holds at most {MAX_SCREENS} screens"));
            return;
        }
        session.contract.screens.push(SavedScreen {
            position: screen.display_position,
            screen_id,
            name: screen.name.clone(),
            settings: screen.settings.clone(),
        });
        session.contract.revision += 1;
        session.active = true;
        state.message = format!("{} is now remembered", screen.name);
        state.diagnostics.record("session", &state.message);
    }

    /// Remove a screen for good: from the session that remembers it, and its display.
    fn request_remove_screen(shared: &Arc<Mutex<State>>, screen_id: ScreenId) {
        let serving = shared.lock().ok().map(|mut state| {
            let owner = state
                .sessions
                .owner(screen_id)
                .map(|session| session.contract.session_id.clone());
            if let Some(session) = owner.and_then(|id| state.sessions.session_mut(&id)) {
                session
                    .contract
                    .screens
                    .retain(|s| s.screen_id != screen_id);
                session.contract.revision += 1;
            }
            state.send(HostUiCommand::RemoveScreen(screen_id));
            state.session_commands.is_some()
        });
        if serving == Some(false) {
            remove_host_screen(shared, screen_id);
        }
    }

    /// Create or remove a session's virtual screens. The saved config is kept either way.
    fn set_session_active(shared: &Arc<Mutex<State>>, session_id: &str, active: bool) {
        let mut orphaned = Vec::new();
        if let Ok(mut state) = shared.lock() {
            let Some(session) = state.sessions.session(session_id).cloned() else {
                return;
            };
            if session.active == active {
                return;
            }
            if active {
                let missing = session
                    .contract
                    .screens
                    .iter()
                    .filter(|saved| state.screen(saved.screen_id).is_none())
                    .cloned()
                    .collect::<Vec<_>>();
                if state.screens.len() + missing.len() > MAX_SCREENS {
                    state.alert(format!(
                        "Activating {} would exceed {MAX_SCREENS} live screens",
                        session.contract.display_name()
                    ));
                    return;
                }
                for saved in missing {
                    let Ok(pin) = random_pin() else { continue };
                    let mut screen =
                        HostScreen::new(saved.screen_id, saved.name, saved.settings, pin);
                    screen.display_position = saved.position;
                    screen.status = "Prepared · restored session".into();
                    state.screens.push(screen);
                    state.send(HostUiCommand::AddScreen(saved.screen_id));
                }
            } else {
                for saved in &session.contract.screens {
                    if state.screen(saved.screen_id).is_some() {
                        state.send(HostUiCommand::RemoveScreen(saved.screen_id));
                        if state.session_commands.is_none() {
                            orphaned.push(saved.screen_id);
                        }
                    }
                }
            }
            if let Some(session) = state.sessions.session_mut(session_id) {
                session.active = active;
            }
            state.message = format!(
                "{} {}",
                session.contract.display_name(),
                if active {
                    "activated · screens created"
                } else {
                    "deactivated · screens removed, config kept"
                }
            );
            state.diagnostics.record("session", &state.message);
        }
        for screen_id in orphaned {
            remove_host_screen(shared, screen_id);
        }
    }

    fn delete_session(shared: &Arc<Mutex<State>>, session_id: &str) {
        set_session_active(shared, session_id, false);
        if let Ok(mut state) = shared.lock() {
            state
                .sessions
                .sessions
                .retain(|s| s.contract.session_id != session_id);
            state.expanded.remove(session_id);
            state.message = "Session removed from this Mac".into();
            state.diagnostics.record("session", &state.message);
        }
    }

    /// Copy what the live screens are into the sessions that remember them.
    /// Screens of inactive sessions are not live and keep their saved config.
    fn persist_sessions(state: &mut State) -> Result<()> {
        for screen in &mut state.screens {
            if let Some(id) = screen.display_id {
                if let Some(position) = mac::display_position(id) {
                    screen.display_position = Some(position);
                }
            }
        }
        for session in &mut state.sessions.sessions {
            let previous = bridge_core::sessions::contract_snapshot(&session.contract)?;
            for saved in &mut session.contract.screens {
                if let Some(screen) = state
                    .screens
                    .iter()
                    .find(|s| s.screen_id == saved.screen_id && !s.removing)
                {
                    saved.position = screen.display_position.or(saved.position);
                    saved.name = screen.name.clone();
                    saved.settings = screen.settings.clone();
                }
            }
            if previous != bridge_core::sessions::contract_snapshot(&session.contract)? {
                session.contract.revision = session.contract.revision.saturating_add(1);
            }
        }
        let snapshot = bridge_core::sessions::snapshot(&state.sessions)?;
        if snapshot != state.storage_snapshot {
            #[cfg(not(test))]
            state.sessions.save()?;
            state.storage_snapshot = snapshot;
        }
        Ok(())
    }

    fn normalize_stats_target(screen: &mut HostScreen) {
        if screen
            .stats_peer
            .is_some_and(|id| !screen.viewers.iter().any(|peer| peer.id == id))
        {
            screen.stats_peer = None;
            screen.stats_view.reset();
        }
    }

    fn stats_telemetry(screen: &HostScreen) -> Arc<Telemetry> {
        screen
            .viewers
            .iter()
            .find(|peer| Some(peer.id) == screen.stats_peer)
            .map(|peer| peer.telemetry.clone())
            .unwrap_or_else(|| screen.telemetry.clone())
    }

    fn random_pin() -> Result<String> {
        use std::io::Read;
        let mut random = [0; 4];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(format!("{:06}", u32::from_le_bytes(random) % 1_000_000))
    }

    fn remove_host_screen(shared: &Arc<Mutex<State>>, screen_id: ScreenId) {
        if let Ok(mut state) = shared.lock() {
            let name = state
                .screen(screen_id)
                .map(|screen| screen.name.clone())
                .unwrap_or_else(|| format!("Slot {screen_id}"));
            state.screens.retain(|screen| screen.screen_id != screen_id);
            if state.page == Page::Screen(screen_id) {
                state.page = Page::Home;
            }
            state.message = format!("{name} removed");
            state.diagnostics.record("screen", &state.message);
        }
    }

    /// First launch and tests: one temporary screen ready for a PIN.
    fn initial_state(pin: String) -> State {
        let mut screen = HostScreen::new(1, "Temp 1".into(), VideoSettings::default(), pin);
        screen.temp_index = 1;
        State {
            message: "Starting…".into(),
            alerting: false,
            screens: vec![screen],
            page: Page::Home,
            session_commands: None,
            diagnostics: Arc::new(Diagnostics::new("host")),
            sessions: HostStore::new().expect("System randomness unavailable"),
            expanded: HashSet::new(),
            storage_snapshot: String::new(),
            power_notice: String::new(),
            settings: HostSettings::default(),
            host_name: "This Mac".into(),
        }
    }

    /// Screens of active sessions come back; temporary ones are forgotten on quit.
    fn restored_state(store: HostStore) -> State {
        let mut state = initial_state(random_pin().expect("System randomness is unavailable"));
        if !store.sessions.is_empty() {
            state.screens.clear();
            for session in store.sessions.iter().filter(|s| s.active) {
                for saved in &session.contract.screens {
                    if state.screens.len() >= MAX_SCREENS || state.screen(saved.screen_id).is_some()
                    {
                        continue;
                    }
                    let mut screen = HostScreen::new(
                        saved.screen_id,
                        saved.name.clone(),
                        saved.settings.clone(),
                        random_pin().expect("System randomness unavailable"),
                    );
                    screen.display_position = saved.position;
                    screen.status = "Prepared · restored session".into();
                    state.screens.push(screen);
                }
            }
        } else if let Some(screen) = state.screens.first_mut() {
            // Keep the first temporary screen clear of any slot a session may take later.
            screen.screen_id = store.free_screen_id([]);
        }
        state.expanded = store
            .sessions
            .first()
            .map(|s| s.contract.session_id.clone())
            .into_iter()
            .collect();
        state.sessions = store;
        state
    }

    include!("ui.rs");

    pub fn main() {
        let mut initial = match HostStore::load() {
            Ok(store) => restored_state(store),
            Err(error) => {
                eprintln!("Cannot load host sessions: {error}");
                return;
            }
        };
        initial.settings = HostSettings::load();
        initial.host_name = bridge_core::config::device_name();
        let state = Arc::new(Mutex::new(initial));
        state
            .lock()
            .unwrap()
            .diagnostics
            .record("host", mac::capture_permission_diagnostic());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let server_state = state.clone();
        let server_thread = std::thread::spawn(move || {
            if let Err(error) = run_server(server_state.clone(), shutdown_rx) {
                set_alert(&server_state, format!("Server could not start: {error:#}"));
            }
        });
        let app_shutdown = shutdown.clone();
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
                let widget_size = size(px(480.0), px(720.0));
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
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_decorations: Some(WindowDecorations::Client),
                        window_min_size: Some(size(px(400.0), px(320.0))),
                        ..TitleBar::window_options()
                    },
                    cx,
                    |window, cx| {
                        window.set_window_title("AirBridge · Host");
                        let view = cx.new(|_| HostView::new(state.clone()));
                        let controls_id = window.window_handle().window_id();
                        cx.on_window_closed(move |cx, closed_id| {
                            if closed_id == controls_id {
                                cx.quit();
                            }
                        })
                        .detach();
                        let view_weak = view.downgrade();
                        cx.spawn(async move |cx| {
                            loop {
                                cx.background_executor()
                                    .timer(Duration::from_millis(250))
                                    .await;
                                if view_weak.update(cx, |this, cx| this.refresh(cx)).is_err() {
                                    break;
                                }
                            }
                        })
                        .detach();
                        view
                    },
                )
                .expect("Cannot open AirBridge window");
                cx.activate(true);
            });
        let _ = shutdown.send(true);
        let _ = server_thread.join();
    }
    include!("service.rs");
    fn run_server(shared: Arc<Mutex<State>>, shutdown: watch::Receiver<bool>) -> Result<()> {
        service::run(shared, shutdown)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn open(
            cx: &mut gpui_kit::TestAppContext,
            state: &Arc<Mutex<State>>,
        ) -> gpui_kit::AnyWindowHandle {
            cx.update(gpui_kit::init);
            let state = state.clone();
            cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                    cx.new(|_| HostView::new(state))
                })
                .unwrap()
                .0
            })
        }

        #[gpui_kit::test]
        fn stats_and_logs_exist_only_in_the_screen_view_and_closing_releases_stats(
            cx: &mut gpui_kit::TestAppContext,
        ) {
            use gpui_kit::test::TestWindowExt;
            let (commands, mut requests) = mpsc::channel(16);
            let mut initial = initial_state("123456".into());
            initial.session_commands = Some(commands);
            let state = Arc::new(Mutex::new(initial));
            let controls = open(cx, &state);
            controls
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find("host-stats").is_none());
                    assert!(window.try_find("host-logs").is_none());
                    assert!(window.try_find("host-settings").is_some());
                    window.click(("host-screen", 1_usize), cx);
                })
                .unwrap();
            assert_eq!(state.lock().unwrap().page, Page::Screen(1));
            controls
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find("host-logs").is_some());
                    window.click("host-stats", cx);
                    window.click("host-stats", cx);
                })
                .unwrap();
            assert!(state.lock().unwrap().screens[0].stats_view.open);
            assert!(matches!(
                requests.try_recv(),
                Ok(HostUiCommand::Diagnostics(1, true))
            ));
            assert!(
                requests.try_recv().is_err(),
                "refocusing does not restart diagnostics again"
            );
            cx.update(|cx| {
                let inspector = cx
                    .windows()
                    .into_iter()
                    .find(|handle| handle.window_id() != controls.window_id())
                    .unwrap();
                inspector
                    .update(cx, |_, window, _| window.remove_window())
                    .unwrap();
            });
            assert!(!state.lock().unwrap().screens[0].stats_view.open);
            assert!(matches!(
                requests.try_recv(),
                Ok(HostUiCommand::Diagnostics(1, false))
            ));
            assert!(requests.try_recv().is_err());
        }

        #[gpui_kit::test]
        fn home_lists_temporary_screens_and_the_screen_view_edits_and_removes(
            cx: &mut gpui_kit::TestAppContext,
        ) {
            use gpui_kit::test::TestWindowExt;
            let state = Arc::new(Mutex::new(initial_state("123456".into())));
            let handle = open(cx, &state);
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    window.click("host-add-temporary", cx);
                })
                .unwrap();
            assert_eq!(state.lock().unwrap().screens.len(), 2);
            assert_eq!(state.lock().unwrap().screens[1].screen_id, 2);
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    window.click(("host-screen", 2_usize), cx);
                })
                .unwrap();
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(window.try_find("host-add-temporary").is_none());
                    window.click("vsync", cx);
                })
                .unwrap();
            assert!(!state.lock().unwrap().screens[1].settings.vsync);
            assert!(state.lock().unwrap().screens[1].edited.is_some());
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    window.click("host-remove-screen", cx);
                })
                .unwrap();
            assert_eq!(state.lock().unwrap().screens.len(), 1);
            assert_eq!(state.lock().unwrap().page, Page::Home);
        }

        #[test]
        fn recreated_screens_reuse_their_slot_and_sessions_keep_config_while_inactive() {
            let mut s = initial_state("123456".into());
            assert_eq!(s.screens[0].pin, "123456");
            s.screens[0].stats_peer = Some(999);
            s.screens[0].stats_view.freeze_after = Some(20);
            normalize_stats_target(&mut s.screens[0]);
            assert!(s.screens[0].stats_peer.is_none());
            assert!(s.screens[0].stats_view.freeze_after.is_none());

            let second = add_host_screen(&mut s, None, VideoSettings::default()).unwrap();
            assert_eq!(second, 2);
            assert_eq!(s.screens[1].pin.len(), 6);
            assert_eq!(slot_label(&s, &s.screens[1]), "T-2");
            let shared = Arc::new(Mutex::new(s));
            remove_host_screen(&shared, second);
            let mut s = shared.lock().unwrap().clone();
            // The released slot is handed out again instead of counting upward.
            for _ in 0..3 {
                let id = add_host_screen(&mut s, None, VideoSettings::default()).unwrap();
                assert_eq!(id, 2);
                s.screens.retain(|screen| screen.screen_id != id);
            }

            let session = s.sessions.create("desk".into(), "", vec![]).unwrap();
            let mut custom = VideoSettings::default();
            custom.width = 2560;
            custom.height = 1440;
            let remembered = add_host_screen(&mut s, Some(&session), custom).unwrap();
            assert_eq!(slot_label(&s, s.screen(remembered).unwrap()), "Slot A");
            move_to_session(&mut s, 1, &session);
            assert_eq!(slot_label(&s, s.screen(1).unwrap()), "Slot B");
            persist_sessions(&mut s).unwrap();

            let shared = Arc::new(Mutex::new(s));
            set_session_active(&shared, &session, false);
            {
                let s = shared.lock().unwrap();
                assert!(
                    s.screens.is_empty(),
                    "inactive sessions have no live screens"
                );
                let saved = &s.sessions.session(&session).unwrap().contract.screens;
                assert_eq!(saved.len(), 2, "but their config is kept");
                assert_eq!(saved[0].settings.width, 2560);
            }
            {
                let mut s = shared.lock().unwrap();
                persist_sessions(&mut s).unwrap();
                assert_eq!(
                    s.sessions.session(&session).unwrap().contract.screens.len(),
                    2
                );
            }
            set_session_active(&shared, &session, true);
            {
                let s = shared.lock().unwrap();
                assert_eq!(s.screens.len(), 2);
                assert_eq!(s.screen(remembered).unwrap().settings.width, 2560);
            }
            request_remove_screen(&shared, remembered);
            let s = shared.lock().unwrap();
            assert_eq!(s.screens.len(), 1);
            assert_eq!(
                s.sessions.session(&session).unwrap().contract.screens.len(),
                1
            );
            // The freed slot is the next one handed out.
            assert_eq!(
                s.sessions
                    .free_screen_id(s.screens.iter().map(|x| x.screen_id)),
                remembered
            );
        }

        #[test]
        fn restart_restores_only_active_session_screens() {
            let mut store = HostStore::new().unwrap();
            let saved = |screen_id| SavedScreen {
                position: None,
                screen_id,
                name: format!("Screen {screen_id}"),
                settings: VideoSettings::default(),
            };
            store
                .create("desk".into(), "", vec![saved(1), saved(2)])
                .unwrap();
            store.create("tv".into(), "", vec![saved(3)]).unwrap();
            store.sessions[1].active = false;
            let state = restored_state(store);
            assert_eq!(
                state
                    .screens
                    .iter()
                    .map(|s| s.screen_id)
                    .collect::<Vec<_>>(),
                [1, 2],
                "no temporary screen and no inactive session screen comes back"
            );
            let empty = restored_state(HostStore::new().unwrap());
            assert_eq!(empty.screens.len(), 1);
            assert!(empty.sessions.owner(empty.screens[0].screen_id).is_none());
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    app::main();
}
