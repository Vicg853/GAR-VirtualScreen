#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported.rs"]
mod linux;

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
            AddScreenRequest, ChangeScreenSettingsRequest, ClientHello, ClientMessage, HostMessage,
            MAX_SCREENS, MediaKind, MediaLane, MediaState, MediaStreamHello, RequestId,
            ScreenConfig, ScreenId, StreamGeneration, VERSION, control_reader,
            receive_debug_media_or_end, receive_json, receive_media_or_end, send_control,
        },
        security::{authenticate_viewer, client_endpoint},
        telemetry::{
            ClockEstimate, Outcome, Stage, StatsView, StreamStats, Telemetry, TimingView,
            TraceToken, now_us,
        },
    };
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, ResizeEdge,
        Window, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, bounds, div, point,
        prelude::*, px, rgb, size,
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
        AddScreen,
        ChangeScreenSettings(ScreenId, u64, VideoSettings),
        RemoveScreen(ScreenId),
        RetryScreen(ScreenId),
        Diagnostics(ScreenId, bool),
        Burst(ScreenId),
    }

    enum PendingAction {
        Add,
        Settings(ScreenId, u64),
        Remove(ScreenId),
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
        events: mpsc::UnboundedSender<LaneNotice>,
    }
    impl Drop for LaneRegistration {
        fn drop(&mut self) {
            if let Ok(mut active) = self.active.lock() {
                active.remove(&self.key);
            }
            if std::thread::panicking() {
                let (id, generation, lane) = self.key;
                let _ = self.events.send(LaneNotice::Failed(
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
        pin: String,
        manual_address: String,
        edit_address: bool,
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
        fn select_host(&mut self, name: &str) {
            if let Some(index) = self.hosts.iter().position(|host| host.name == name) {
                self.selected_host = index;
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

    struct ViewerView {
        state: Arc<Mutex<State>>,
        commands: mpsc::UnboundedSender<Command>,
        focus: FocusHandle,
        rendered: bool,
    }

    fn resize_from_border(event: &gpui::MouseDownEvent, window: &mut Window) {
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
        commands: mpsc::UnboundedSender<Command>,
        action: fn(&mut State),
        cx: &mut Context<ViewerView>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(0x35517a))
            .cursor_pointer()
            .child(label)
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
                            .send(Command::ChangeScreenSettings(screen_id, revision, settings));
                    }
                }
                cx.notify();
            }))
    }

    impl Render for ViewerView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.rendered {
                self.rendered = true;
                if let Ok(state) = self.state.lock() {
                    state.diagnostics.record(
                        "ui",
                        format!(
                            "Controls window rendering at {:?}",
                            window.window_bounds().get_bounds()
                        ),
                    );
                }
            }
            if let Ok(mut state) = self.state.lock() {
                for s in &mut state.screens {
                    if s.stats_view.open {
                        s.stats_view.refresh(&s.telemetry);
                    }
                }
            }
            let state = self.state.lock().unwrap().clone();
            let page = state.page();
            let commands = self.commands.clone();
            let active_screen = state
                .screens
                .iter()
                .find(|screen| Some(screen.config.screen_id) == state.navigation.selected)
                .cloned();
            let (decoded, skipped, decoder, frame_error) = active_screen
                .as_ref()
                .and_then(|screen| screen.frame.lock().ok())
                .map(|frame| {
                    (
                        frame.frames,
                        frame.skipped,
                        frame.decoder.clone(),
                        frame.error.clone(),
                    )
                })
                .unwrap_or_default();
            let hosts = state
                .hosts
                .iter()
                .enumerate()
                .map(|(index, host)| {
                    let shared = self.state.clone();
                    let selected = index == state.selected_host;
                    let host_name = host.name.clone();
                    div()
                        .id(("host", index))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(if selected { 0x365986 } else { 0x28354b }))
                        .border_1()
                        .border_color(rgb(if selected { 0x6fd3ff } else { 0x344158 }))
                        .cursor_pointer()
                        .child(format!("{}{}", if selected { "● " } else { "" }, host.name))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                state.select_host(&host_name);
                            }
                            cx.notify();
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
                    div()
                        .id(("viewer-screen", screen_id as usize))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(if selected { 0x365986 } else { 0x28354b }))
                        .border_1()
                        .border_color(rgb(if selected { 0x6fd3ff } else { 0x344158 }))
                        .cursor_pointer()
                        .child(format!(
                            "{}{} · {}",
                            if selected { "● " } else { "" },
                            screen.config.name,
                            screen.status
                        ))
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
                    div()
                        .id(("preset", index))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(if index == state.selected_preset {
                            0x365986
                        } else {
                            0x28354b
                        }))
                        .border_1()
                        .border_color(rgb(if index == state.selected_preset {
                            0x6fd3ff
                        } else {
                            0x344158
                        }))
                        .cursor_pointer()
                        .child(format!(
                            "{}{}",
                            if index == state.selected_preset {
                                "● "
                            } else {
                                ""
                            },
                            preset.name
                        ))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                if state.screens.iter().any(|s| {
                                    Some(s.config.screen_id) == state.navigation.selected
                                        && s.removing
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
                                    let _ = commands.send(Command::ChangeScreenSettings(
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
                    .bg(rgb(0x28354b))
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
            let add_state = self.state.clone();
            let add_commands = self.commands.clone();
            let active_diagnostics = active_screen
                .as_ref()
                .map(|screen| screen.diagnostics.clone())
                .unwrap_or_else(|| state.diagnostics.clone());
            let stats_panel = if let Some(screen) = &active_screen {
                let id = screen.config.screen_id;
                let mut panel = div()
                    .id("viewer-stats-panel")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .id("viewer-stats-toggle")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x28354b))
                            .cursor_pointer()
                            .child(if screen.stats_view.open {
                                "Stats ▴ · instrumented stream"
                            } else {
                                "Stats ▾"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Ok(mut state) = this.state.lock() {
                                    if let Some(s) =
                                        state.screens.iter_mut().find(|s| s.config.screen_id == id)
                                    {
                                        let enabled = !s.stats_view.open;
                                        s.stats_view.open = enabled;
                                        s.stats_view.frozen = None;
                                        let _ =
                                            this.commands.send(Command::Diagnostics(id, enabled));
                                    }
                                }
                                cx.notify();
                            })),
                    );
                if screen.stats_view.open {
                    panel = panel
                        .child("Temporary diagnostics · opening/closing restarts this stream")
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    div()
                                        .id("viewer-timing-frames")
                                        .px_2()
                                        .py_1()
                                        .bg(rgb(if screen.stats_view.mode == TimingView::Frames {
                                            0x35517a
                                        } else {
                                            0x28354b
                                        }))
                                        .cursor_pointer()
                                        .child("Frame rows")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.config.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Frames;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("viewer-timing-averages")
                                        .px_2()
                                        .py_1()
                                        .bg(rgb(
                                            if screen.stats_view.mode == TimingView::Averages {
                                                0x35517a
                                            } else {
                                                0x28354b
                                            },
                                        ))
                                        .cursor_pointer()
                                        .child("Stage averages")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.config.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Averages;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("viewer-timing-freeze")
                                        .px_2()
                                        .py_1()
                                        .bg(rgb(0x28354b))
                                        .cursor_pointer()
                                        .child(if screen.stats_view.frozen.is_some() {
                                            "Resume"
                                        } else {
                                            "Freeze"
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.config.screen_id == id)
                                                {
                                                    s.stats_view.frozen =
                                                        if s.stats_view.frozen.is_some() {
                                                            None
                                                        } else {
                                                            Some(s.telemetry.snapshot())
                                                        };
                                                    s.stats_view.freeze_after = None;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("viewer-timing-burst")
                                        .px_2()
                                        .py_1()
                                        .bg(rgb(0x28354b))
                                        .cursor_pointer()
                                        .child("Capture next 20")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.config.screen_id == id)
                                                {
                                                    s.stats_view.frozen = None;
                                                    s.stats_view.freeze_after = Some(
                                                        s.telemetry.snapshot().averages
                                                            [Stage::Capture as usize]
                                                            .count
                                                            + 20,
                                                    );
                                                }
                                                let _ = this.commands.send(Command::Burst(id));
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(
                            screen
                                .stats_view
                                .lines(&screen.telemetry)
                                .into_iter()
                                .map(|line| div().text_sm().child(line)),
                        )
                        .child(
                            div()
                                .id("viewer-timing-table")
                                .h(px(220.))
                                .overflow_scroll()
                                .text_sm()
                                .children(
                                    screen
                                        .stats_view
                                        .table(&screen.telemetry)
                                        .into_iter()
                                        .enumerate()
                                        .map(|(index, row)| {
                                            div()
                                                .flex()
                                                .gap_1()
                                                .bg(rgb(if index == 0 {
                                                    0x28354b
                                                } else {
                                                    0x0d1420
                                                }))
                                                .children(row.into_iter().map(|cell| {
                                                    div()
                                                        .w(px(175.))
                                                        .flex_shrink_0()
                                                        .p_1()
                                                        .child(cell)
                                                }))
                                        }),
                                ),
                        );
                }
                panel
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
                    .bg(rgb(0x0d1420))
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
            let logs_toggle = div()
                .id("logs-menu")
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x28354b))
                .cursor_pointer()
                .child(if state.show_logs {
                    "Logs ▴"
                } else {
                    "Logs ▾"
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.show_logs = !state.show_logs;
                    }
                    cx.notify();
                }));
            let list_toggle = div()
                .id("viewer-screen-view-toggle-list")
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x35517a))
                .cursor_pointer()
                .child("Selected stream details →")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.view = ScreenView::Details;
                    }
                    cx.notify();
                }));
            let details_toggle = div()
                .id("viewer-screen-view-toggle-details")
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x35517a))
                .cursor_pointer()
                .child("← Stream list")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        let ids = state
                            .screens
                            .iter_mut()
                            .filter(|s| s.stats_view.open)
                            .map(|s| {
                                s.stats_view.open = false;
                                s.config.screen_id
                            })
                            .collect::<Vec<_>>();
                        for id in ids {
                            let _ = this.commands.send(Command::Diagnostics(id, false));
                        }
                        state.navigation.back();
                        state.show_codecs = false;
                        state.show_logs = false;
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
                                .text_color(rgb(0xa8b8cf))
                                .child("SCREEN STREAMS"),
                        )
                        .child(div().flex().flex_col().gap_1().children(screen_cards))
                        .child(
                            div()
                                .id("add-screen-stream")
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(if state.connected { 0x35517a } else { 0x28354b }))
                                .cursor_pointer()
                                .child(if state.adding {
                                    "Adding stream…"
                                } else if state.connected {
                                    "+ Add stream"
                                } else {
                                    "Connect to add a stream"
                                })
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if add_state
                                        .lock()
                                        .map(|state| state.connected && !state.adding)
                                        .unwrap_or(false)
                                    {
                                        let _ = add_commands.send(Command::AddScreen);
                                    }
                                    cx.notify();
                                })),
                        )
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
                            .map(|screen| format!("Stream details · {} · {:?} · generation {}", screen.config.name, screen.config.codec, screen.config.generation))
                            .unwrap_or_else(|| "No active screen stream".into()),
                    )
                    .child(div().flex().flex_wrap().gap_2()
                        .when(active_screen.as_ref().is_some_and(|s| !s.removing), |row| row
                            .child(div().id("retry-stream").px_3().py_2().rounded_md().bg(rgb(0x35517a)).cursor_pointer()
                                .child(if active_screen.as_ref().is_some_and(|s| s.terminal) { "Retry stream" } else { "Restart stream" })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(id) = this.state.lock().ok().and_then(|s| s.navigation.selected) { let _ = this.commands.send(Command::RetryScreen(id)); }
                                    cx.notify();
                                })))
                            .child(div().id("remove-stream").px_3().py_2().rounded_md().bg(rgb(0x573846)).cursor_pointer().child("Remove stream")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(id) = this.state.lock().ok().and_then(|s| s.navigation.selected) { let _ = this.commands.send(Command::RemoveScreen(id)); }
                                    cx.notify();
                                })))))
                    .child(logs_toggle)
                    .child(logs)
                    .when(active_screen.as_ref().is_some_and(|s| !s.terminal && !s.removing), |panel| panel.child(
                        div()
                            .id("show-display")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x28354b))
                            .cursor_pointer()
                            .child("Show display window")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let result = this.state.lock().ok()
                                    .and_then(|state| state.screens.iter().find(|screen| Some(screen.config.screen_id) == state.navigation.selected).map(|screen| screen.video_window.clone()))
                                    .map(|window| window.show());
                                if let Some(Err(error)) = result {
                                    message(&this.state, format!("Cannot show display: {error:#}"));
                                }
                                cx.notify();
                            })),
                    ))
                    .when(active_screen.as_ref().is_some_and(|s| !s.removing), |panel| panel
                    .child(div().text_sm().text_color(rgb(0xa8b8cf)).child(if state.connected { "PRESETS · selected stream" } else { "PRESETS · next connection" }))
                    .child(div().flex().flex_wrap().gap_2().children(presets))
                    .child(
                        div()
                            .id("codec-menu")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x28354b))
                            .cursor_pointer()
                            .child(if state.show_codecs { "Video settings ▴" } else { "Video settings ▾" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Ok(mut state) = this.state.lock() {
                                    state.show_codecs = !state.show_codecs;
                                }
                                cx.notify();
                            })),
                    )
                    .child(codec_panel)
                    .child(stats_panel)
)
                    .child(format!("Received {} · host sent {} · host drops {} · decoded {} · skipped {} · {} · {:.1} MB · {} kbps{}", active_screen.as_ref().map(|screen| screen.received_frames).unwrap_or(state.received_frames), active_screen.as_ref().map(|screen| screen.host_frames_sent).unwrap_or_default(), active_screen.as_ref().map(|screen| screen.host_frames_dropped).unwrap_or_default(), decoded, skipped, if decoder.is_empty() { "decoder initializing" } else { decoder.as_str() }, active_screen.as_ref().map(|screen| screen.bytes).unwrap_or(state.bytes) as f64 / 1_000_000.0, active_screen.as_ref().map(|screen| screen.bitrate_kbps).unwrap_or_default(),
                        frame_error.map(|e| format!(" · {e}")).unwrap_or_default()))
            } else if page == ControlsPage::Connect {
                div()
                    .id("viewer-next-connection-settings")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("PRESETS · next connection")
                    .child(div().flex().flex_wrap().gap_2().children(presets))
                    .child(
                        div()
                            .id("connection-video-settings")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x28354b))
                            .cursor_pointer()
                            .child(if state.show_codecs {
                                "Video settings ▴"
                            } else {
                                "Video settings ▾"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Ok(mut state) = this.state.lock() {
                                    state.show_codecs = !state.show_codecs;
                                }
                                cx.notify();
                            })),
                    )
                    .child(codec_panel)
                    .child(logs_toggle)
                    .child(logs)
            } else {
                div().id("viewer-screen-details-hidden")
            };
            div().size_full().min_h_0().track_focus(&self.focus).on_key_down(cx.listener(Self::on_key))
                .on_mouse_down(MouseButton::Left, |event, window, _| resize_from_border(event, window))
                .bg(rgb(0x141c2b)).border_1().border_color(rgb(0x36465f))
                .text_color(rgb(0xf2f5fb)).flex().flex_col()
                .child(div().h(px(42.0)).px_3().flex().items_center().justify_between().bg(rgb(0x1d2940))
                    .child(div().flex_1().child("AIRBRIDGE  ·  VIEWER")
                        .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()))
                    .child(div().id("minimize").px_2().cursor_pointer().child("−")
                        .on_click(cx.listener(|_, _, window, _| window.minimize_window())))
                    .child(div().id("close").px_2().cursor_pointer().child("×")
                        .on_click(cx.listener(|_, _, _, cx| cx.quit()))))
                .child(div().px_3().py_2().bg(rgb(0x203149)).text_sm().child(state.message.clone()))
                .child(div().id("viewer-settings-scroll").flex_1().min_h_0().overflow_y_scroll().p_3().flex().flex_col().gap_3()
                    .when(page == ControlsPage::Connect, |panel| panel
                    .child(div().text_sm().text_color(rgb(0xa8b8cf)).child("DISCOVERED HOSTS"))
                    .child(div().flex().flex_col().gap_1().children(hosts))
                    .child(format!("PIN: {}  ·  {}", if state.pin.is_empty() { "_" } else { &state.pin }, if state.edit_address { "manual address input active" } else { "type six digits" }))
                    .child(format!("Manual address: {}", if state.manual_address.is_empty() { "not set" } else { &state.manual_address }))
                    .child(div().id("edit-address").px_3().py_2().rounded_md().bg(rgb(if state.edit_address { 0x365986 } else { 0x28354b }))
                        .cursor_pointer().child(if state.edit_address { "Typing address · click for PIN" } else { "Typing PIN · click for address" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Ok(mut state) = this.state.lock() { state.edit_address = !state.edit_address; }
                            cx.notify();
                        })))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(div().id("connect").px_4().py_2().rounded_md().bg(rgb(0x3777b6)).cursor_pointer().child("Connect")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                let state = connect_state.lock().unwrap().clone();
                                if state.connected || state.connecting { return; }
                                if bridge_core::security::validate_pin(&state.pin).is_err() { message(&connect_state, "Enter the six-digit host PIN"); cx.notify(); return; }
                                let address = if !state.manual_address.is_empty() {
                                    state.manual_address.parse::<SocketAddr>().ok().or_else(|| state.manual_address.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, bridge_core::protocol::PORT)))
                                } else { state.hosts.get(state.selected_host).and_then(|host| host.addresses.first().copied()) };
                                if let Some(address) = address { let _ = connect_commands.send(Command::Connect(address, state.pin, if state.override_host { Some(state.settings) } else { None })); }
                                else { message(&connect_state, "Select a discovered Mac or enter its IP address"); }
                                cx.notify();
                            })))
)
)
                    .when(state.connected || state.connecting, |panel| panel.child(
                        div().id("disconnect").px_4().py_2().rounded_md().bg(rgb(0x573846)).cursor_pointer()
                            .child(if state.connecting { "Cancel connection" } else { "Disconnect all streams" })
                            .on_click(cx.listener(|this, _, _, _| { let _ = this.commands.send(Command::Disconnect); }))))
                    .child(screen_list_panel)
                    .child(stream_details_panel))
        }
    }

    impl ViewerView {
        fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
            if let Ok(mut state) = self.state.lock() {
                if state.connected || state.connecting {
                    return;
                }
                let editing_address = state.edit_address;
                let value = if editing_address {
                    &mut state.manual_address
                } else {
                    &mut state.pin
                };
                if event.keystroke.key == "backspace" {
                    value.pop();
                } else if editing_address {
                    if let Some(character) = &event.keystroke.key_char {
                        if character.len() == 1 && value.len() < 64 {
                            value.push_str(character);
                        }
                    }
                } else if let Some(character) = &event.keystroke.key_char {
                    if character.len() == 1
                        && character.bytes().all(|b| b.is_ascii_digit())
                        && value.len() < 6
                    {
                        value.push_str(character);
                    }
                }
            }
            cx.notify();
        }
    }

    fn open_controls(
        cx: &mut App,
        state: Arc<Mutex<State>>,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Result<WindowHandle<ViewerView>> {
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
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(330.0), px(300.0))),
                app_id: Some("airbridge-viewer".into()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("AirBridge Controls");
                let focus = cx.focus_handle();
                window.focus(&focus);
                let view = cx.new(|_| ViewerView {
                    state,
                    commands,
                    focus,
                    rendered: false,
                });
                let weak = view.downgrade();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        if weak.update(cx, |_, cx| cx.notify()).is_err() {
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
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            loop {
                let command = tokio::select! {
                    _ = shutdown.changed() => break,
                    command = commands.recv() => { let Some(command) = command else { break; }; command }
                };
                if let Command::Connect(address, pin, settings) = command {
                    if let Ok(mut state) = shared.lock() { state.connecting = true; }
                    let result = connect(address, pin, settings, shared.clone(), &mut commands, shutdown.clone()).await;
                    clear_client_session(&shared);
                    if let Err(error) = result { message(&shared, format!("Connection failed: {error:#}")); }
                    if *shutdown.borrow() { break; }
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
        let diagnostics = Arc::new(Diagnostics::new(&format!("viewer-screen-{screen_id}")));
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
            Some(screen)
                if screen.config.instrumented == config.instrumented
                    && !screen.terminal
                    && !screen
                        .frame
                        .lock()
                        .map(|frame| frame.closed)
                        .unwrap_or(true) =>
            {
                screen
            }
            Some(screen) => {
                screen.cancel.send_replace(true);
                let _ = screen.video_window.close();
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
        commands: &mut mpsc::UnboundedReceiver<Command>,
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
                    Some(command) => queued.push_back(command),
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
            screen.status = "Removing screen…".into();
            screen.diagnostics.record(
                "lifecycle",
                "Closing local display and requesting host removal",
            );
            Some(screen.clone())
        });
        let Some(screen) = screen else {
            return Ok(());
        };
        screen.cancel.send_replace(true);
        let _ = screen.video_window.close();
        let request_id = next_request_id(shared);
        pending.insert(request_id, PendingAction::Remove(screen_id));
        send_control(
            control,
            &ClientMessage::RemoveScreen {
                request_id,
                screen_id,
            },
        )
        .await
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
                screen.diagnostics.record("lifecycle", &detail);
                if screen.status != "Video lane closed" || detail == "Video lane closed" {
                    return None;
                }
            }
            screen.terminal = true;
            screen.status = detail.clone();
            screen.diagnostics.record("lifecycle", &detail);
            let screen = screen.clone();
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
        events: mpsc::UnboundedSender<LaneNotice>,
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
        events: mpsc::UnboundedSender<LaneNotice>,
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
                        .submit_debug(bytes, header.keyframe, header.sequence, trace.clone())
                        .await
                } else {
                    screen.video_window.submit(bytes, header.keyframe).await
                }
            };
            tokio::select! {
                _ = cancel.changed() => { if DEBUG {if let Some(t)=&trace {t.mark(Stage::DecodeQueue,now_us(),Some(now_us()),Outcome::Cancelled,"Stream replaced or closed");}} let _ = media_recv.stop(0u32.into()); return Ok(()); }
                result = submit => {if DEBUG {if let (Some(t),Err(e))=(&trace,&result){t.mark(Stage::DecodeQueue,now_us(),Some(now_us()),Outcome::Failed,&e.to_string());}} result?},
            }
            if first_frame {
                first_frame = false;
                let _ = events.send(LaneNotice::FirstFrame(screen_id, generation));
            }
        }
    }

    async fn receive_lanes(
        connection: quinn::Connection,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<LaneNotice>,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        let active = Arc::new(Mutex::new(
            HashSet::<(ScreenId, StreamGeneration, MediaLane)>::new(),
        ));
        loop {
            tokio::select! {
                incoming = connection.accept_uni() => {
                    let Ok(mut stream) = incoming else { break; };
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
                        let key = (screen_id, generation, lane);
                        if let Ok(mut active) = active.lock() {
                            if !active.insert(key) {
                                let _ = stream.stop(0u32.into());
                                return (0, 0, lane, Err(anyhow::anyhow!("Duplicate video lane for screen {screen_id}, generation {generation}")));
                            }
                        }
                        let _registration = LaneRegistration { key, active: active.clone(), events: events.clone() };
                        let outcome = match wait_for_client_screen(&shared, screen_id, generation).await {
                            Ok(screen) => {
                                let _ = events.send(LaneNotice::Opened(screen_id, generation, lane));
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
                            Ok(()) => { if lane!=MediaLane::Diagnostics {let _ = events.send(LaneNotice::Closed(screen_id, generation, lane));} },
                            Err(error) => { let _ = events.send(LaneNotice::Failed(screen_id, generation, lane, format!("{error:#}"))); },
                        },
                        Some(Err(error)) => {
                            let _ = events.send(LaneNotice::Failed(0, 0, MediaLane::Video, format!("Media lane task failed: {error}")));
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
        commands: &mut mpsc::UnboundedReceiver<Command>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let settings = settings.map(|mut settings| {
            settings.audio = false;
            settings
        });
        if let Some(settings) = &settings {
            settings.validate()?;
        }
        bridge_core::security::validate_pin(&pin)?;
        message(&shared, format!("Connecting to {address}…"));
        let setup = async {
            let endpoint = client_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;
            let connection = tokio::time::timeout(
                Duration::from_secs(8),
                endpoint.connect(address, "airbridge.local")?,
            )
            .await??;
            tokio::time::timeout(
                Duration::from_secs(12),
                authenticate_viewer(&connection, &pin),
            )
            .await
            .map_err(|_| anyhow::anyhow!("PIN entry timed out"))??;
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
            send_control(
                &mut control_send,
                &ClientMessage::Hello(ClientHello {
                    version: VERSION,
                    codecs,
                    initial_settings: settings,
                }),
            )
            .await?;
            let accepted: HostMessage =
                tokio::time::timeout(Duration::from_secs(15), receive_json(&mut control_recv))
                    .await
                    .map_err(|_| anyhow::anyhow!("Host session acceptance timed out"))??;
            let primary = match accepted {
                HostMessage::Accepted { screen } => screen,
                HostMessage::Error { message, .. } => bail!("Host: {message}"),
                _ => bail!("Host sent an invalid session response"),
            };
            Ok::<_, anyhow::Error>((endpoint, connection, control_send, control_recv, primary))
        };
        tokio::pin!(setup);
        let (_endpoint, connection, mut control_send, control_recv, primary) = loop {
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
        if !install_during_session(
            &shared,
            primary.clone(),
            true,
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
        if let Ok(mut state) = shared.lock() {
            state.clock = ClockEstimate::default();
            state.connected = true;
            state.connecting = false;
            state.override_host = false;
            state.settings = primary.settings.clone();
            state.bytes = 0;
            state.received_frames = 0;
        }
        message(
            &shared,
            format!(
                "Connected · {} · {:?} {:?} · {}×{} at {} fps",
                primary.name,
                primary.codec.codec,
                primary.codec.acceleration,
                primary.settings.width,
                primary.settings.height,
                primary.settings.fps,
            ),
        );
        let (lane_events, mut lane_notices) = mpsc::unbounded_channel();
        let mut lane_task = tokio::spawn(receive_lanes(
            connection.clone(),
            shared.clone(),
            lane_events,
        ));
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
        let mut counters = HashMap::<ScreenId, (u64, u64, u64, u64)>::new();
        let mut pending_requests = HashMap::<RequestId, PendingAction>::new();
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
                        counters.retain(|id,_|screens.iter().any(|s|s.config.screen_id==*id));
                        let stats_now=now_us(); let collect=stats_now.saturating_sub(last_stats)>=1_000_000;
                        for screen in screens {
                            if collect && screen.config.instrumented {
                                let (decoded,skipped)=screen.frame.lock().map(|s|(s.frames,s.skipped)).unwrap_or_default();
                                let previous=counters.insert(screen.config.screen_id,(screen.bytes,screen.received_frames,decoded,connection.rtt().as_micros() as u64)).unwrap_or_default();
                                let elapsed=stats_now.saturating_sub(last_stats) as f64/1_000_000.;let rtt=connection.rtt().as_micros() as u64;let transport=connection.stats();
                                screen.telemetry.stats(StreamStats{at_us:stats_now,bitrate_kbps:screen.bytes.saturating_sub(previous.0) as f64*8./elapsed/1000.,fps:screen.received_frames.saturating_sub(previous.1) as f64/elapsed,decoded_fps:Some(decoded.saturating_sub(previous.2) as f64/elapsed),frames:screen.received_frames,dropped:skipped+screen.host_frames_dropped,rtt_us:rtt,jitter_us:rtt.abs_diff(previous.3),lost_packets:transport.path.lost_packets,congestion_events:transport.path.congestion_events,..Default::default()});
                                let clock=shared.lock().map(|s|s.clock).unwrap_or_default();screen.telemetry.expire(stats_now as i64+clock.offset_us,"Stage did not conclude within five seconds");
                            }
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
                            Some(Command::RetryScreen(screen_id)) => {
                                let update = shared.lock().ok().and_then(|mut state| {
                                    let screen = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id && !screen.removing)?;
                                    screen.requested_revision = screen.requested_revision.max(screen.config.revision).saturating_add(1);
                                    screen.status = "Retrying stream…".into();
                                    Some((screen.requested_revision, screen.desired_settings.clone()))
                                });
                                if let Some((revision, settings)) = update {
                                    let request_id = next_request_id(&shared);
                                    pending_requests.insert(request_id, PendingAction::Settings(screen_id, revision));
                                    send_control(&mut control_send, &ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest { request_id, screen_id, revision, settings })).await?;
                                }
                            }
                            Some(Command::AddScreen) => {
                                if shared.lock().map(|state| state.adding).unwrap_or(true) { continue; }
                                let (settings, count) = if let Ok(state) = shared.lock() {
                                    (state.settings.clone(), state.screens.len())
                                } else { (primary.settings.clone(), MAX_SCREENS) };
                                if count >= MAX_SCREENS {
                                    message(&shared, format!("A maximum of {MAX_SCREENS} screens is supported"));
                                    continue;
                                }
                                let request_id = next_request_id(&shared);
                                let mut settings = settings;
                                settings.audio = false;
                                pending_requests.insert(request_id, PendingAction::Add);
                                if let Ok(mut state) = shared.lock() { state.adding = true; }
                                send_control(&mut control_send, &ClientMessage::AddScreen(AddScreenRequest { request_id, settings: Some(settings) })).await?;
                                message(&shared, "Requesting a new host screen…");
                            }
                            Some(Command::ChangeScreenSettings(screen_id, revision, mut settings)) => {
                                let exists = shared.lock().map(|state| state.screens.iter().any(|screen| screen.config.screen_id == screen_id && !screen.removing)).unwrap_or(false);
                                if !exists { continue; }
                                settings.audio = false;
                                let request_id = next_request_id(&shared);
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
                    lane = &mut lane_task => {
                        match lane {
                            Ok(()) => bail!("The media connection closed"),
                            Err(error) => bail!("Media lane manager stopped: {error}"),
                        }
                    }
                    notice = lane_notices.recv() => {
                        match notice {
                            Some(LaneNotice::Opened(screen_id, generation, lane)) => update_client_screen_status(&shared, screen_id, generation, format!("{lane:?} lane opened · waiting for frames")),
                            Some(LaneNotice::FirstFrame(screen_id, generation)) => update_client_screen_status(&shared, screen_id, generation, "Streaming video".into()),
                            Some(LaneNotice::Failed(screen_id, generation, lane, error)) => if screen_id == 0 { message(&shared, format!("Rejected media lane: {error}")); } else if lane == MediaLane::Video { end_client_lane(&shared, screen_id, generation, format!("Video lane failed: {error}")); },
                            Some(LaneNotice::Closed(screen_id, generation, lane)) => if lane == MediaLane::Video { end_client_lane(&shared, screen_id, generation, "Video lane closed".into()); },
                            None => {},
                        }
                    }
                    response = control_messages.recv() => {
                        match response.ok_or_else(|| anyhow::anyhow!("Host control channel ended"))?? {
                            HostMessage::ScreenAdded { request_id, screen } => {
                                let added = request_id.and_then(|id| pending_requests.remove(&id));
                                if matches!(added, Some(PendingAction::Add)) { if let Ok(mut state) = shared.lock() { state.adding = false; } }
                                match install_during_session(&shared, screen.clone(), true, &connection, commands, &mut queued_commands, &mut shutdown).await {
                                    Ok(false) => break,
                                    Ok(true) => message(&shared, format!("{} is ready · opening its independent video window", screen.name)),
                                    Err(error) => message(&shared, format!("Could not open {}: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenSettingsApplied { request_id, screen } => {
                                if let Some(id) = request_id { pending_requests.remove(&id); }
                                match install_during_session(&shared, screen.clone(), false, &connection, commands, &mut queued_commands, &mut shutdown).await {
                                    Ok(false) => break,
                                    Ok(true) => message(&shared, format!("{} settings applied · stream generation {}", screen.name, screen.generation)),
                                    Err(error) => message(&shared, format!("Could not apply {} settings: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenRemoved { request_id, screen_id } => {
                                if let Some(id) = request_id { pending_requests.remove(&id); }
                                pending_requests.retain(|_, action| !matches!(action, PendingAction::Settings(id, _) | PendingAction::Remove(id) if *id == screen_id));
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
                                        Some(PendingAction::Add) => state.adding = false,
                                        Some(PendingAction::Remove(id)) => {
                                            if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == id) {
                                                screen.removing = false;
                                                screen.terminal = true;
                                            }
                                        }
                                        Some(PendingAction::Settings(id, revision)) => {
                                            if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == id && screen.requested_revision == revision) {
                                                screen.desired_settings = screen.config.settings.clone();
                                            }
                                            if state.navigation.selected == Some(id) {
                                                if let Some(settings) = state.screens.iter().find(|screen| screen.config.screen_id == id).map(|screen| screen.desired_settings.clone()) { state.settings = settings; }
                                            }
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
                            HostMessage::Accepted { .. } => message(&shared, "Host sent a second session acceptance; ignoring it"),
                        }
                    }
                }
            }
            Ok(())
        }.await;
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
        lane_task.abort();
        let _ = lane_task.await;
        connection.close(0u32.into(), b"viewer disconnected");
        clear_client_session(&shared);
        if session_result.is_ok() {
            message(&shared, "Disconnected");
        }
        session_result
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
                screen.status = status.clone();
                screen.diagnostics.record("stream", &status);
            }
        }
    }

    fn initial_state(presets: Vec<Preset>) -> State {
        State {
            clock: ClockEstimate::default(),
            hosts: Vec::new(),
            selected_host: 0,
            pin: String::new(),
            manual_address: String::new(),
            edit_address: false,
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
        let (send, receive) = mpsc::unbounded_channel();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let network_state = state.clone();
        let network_thread = std::thread::spawn(move || {
            if let Err(error) = run_network(network_state.clone(), receive, shutdown_rx) {
                message(&network_state, format!("Network error: {error:#}"));
            }
        });
        let browse_state = state.clone();
        let discovery_stop = Arc::new(AtomicBool::new(false));
        let browse_stop = discovery_stop.clone();
        let discovery_thread = std::thread::spawn(move || match browse(browse_stop.clone()) {
            Ok(found) => {
                while !browse_stop.load(Ordering::Acquire) {
                    match found.recv_timeout(Duration::from_millis(250)) {
                        Ok(event) => {
                            if let Ok(mut state) = browse_state.lock() {
                                state.discovery_event(event);
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
        Application::new().run(move |cx: &mut App| {
            cx.on_app_quit(move |_| {
                let _ = app_shutdown.send(true);
                async {}
            })
            .detach();
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let controls = open_controls(cx, state.clone(), send.clone())
                .expect("Cannot open AirBridge controls");
            let _ = controls;
            state.lock().unwrap().diagnostics.record(
                "viewer",
                "Independent native video window and GPUI controls opened",
            );
            cx.activate(true);
        });
        let _ = shutdown.send(true);
        let _ = network_thread.join();
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
        #[tokio::test]
        async fn media_panic_is_scoped_and_unregisters_the_lane() {
            let key = (2, 7, MediaLane::Video);
            let active = Arc::new(Mutex::new(HashSet::from([key])));
            let (send, mut receive) = mpsc::unbounded_channel();
            let registered = active.clone();
            let task = tokio::spawn(async move {
                let _registration = LaneRegistration {
                    key,
                    active: registered,
                    events: send,
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
            end_client_lane(&shared, 1, 3, "Video lane closed".into());
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
            assert_eq!(pending.len(), 1);
            let request: ClientMessage = receive_json(&mut read).await.unwrap();
            assert!(matches!(
                request,
                ClientMessage::RemoveScreen { screen_id: 1, .. }
            ));
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
            let (_commands, receive) = mpsc::unbounded_channel();
            let (stop, stopped) = watch::channel(false);
            stop.send_replace(true);
            run_network(shared.clone(), receive, stopped).unwrap();
            assert!(*cancel.borrow());
            assert_eq!(shared.lock().unwrap().page(), ControlsPage::Connect);
        }

        #[tokio::test]
        async fn connection_can_be_cancelled_during_startup() {
            let shared = state();
            let (send, mut receive) = mpsc::unbounded_channel();
            let (_shutdown, shutdown) = watch::channel(false);
            send.send(Command::Disconnect).unwrap();
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
    app::main();
}
