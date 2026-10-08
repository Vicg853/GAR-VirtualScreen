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
            CodecOption, Preset, VideoSettings, built_in_presets, load_custom_presets,
            save_custom_presets,
        },
        diagnostics::Diagnostics,
        discovery::Advertiser,
        lifecycle::{ScreenNavigation, ScreenView},
        protocol::{
            ClientMessage, ConfigRevision, HostMessage, MAX_SCREENS, MediaLane, MediaState,
            MediaStreamHello, PORT, RequestId, ScreenConfig, ScreenId, StreamGeneration, VERSION,
            control_reader, receive_json, send_control, send_debug_media, send_json, send_media,
        },
        security::server_endpoint,
        telemetry::{Outcome, Stage, StatsView, StreamStats, Telemetry, TimingView, now_us},
    };
    use gpui_kit::component::{
        ActiveTheme, Selectable, Sizable, TitleBar,
        button::{Button, ButtonVariants},
        form::{Field, Form},
        input::{Input, InputState},
    };
    use gpui_kit::{
        App, Bounds, Context, MouseButton, ResizeEdge, Window, WindowBounds, WindowDecorations,
        WindowOptions, bounds, div, point, prelude::*, px, size,
    };
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot, watch};

    #[derive(Clone)]
    struct HostScreen {
        display_id: Option<u32>,
        display_position: Option<bridge_core::sessions::DisplayPosition>,
        pin: String,
        viewers: Vec<ViewerPeer>,
        stats_peer: Option<u64>,
        screen_id: ScreenId,
        name: String,
        revision: ConfigRevision,
        generation: StreamGeneration,
        settings: VideoSettings,
        codec: Option<CodecOption>,
        status: String,
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
    }

    impl HostScreen {
        fn instrumented(&self) -> bool {
            self.stats_view.open || self.viewer_stats
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

    #[derive(Clone)]
    struct State {
        message: String,
        settings: VideoSettings,
        screens: Vec<HostScreen>,
        navigation: ScreenNavigation,
        next_screen_id: ScreenId,
        session_commands: Option<mpsc::Sender<HostUiCommand>>,
        presets: Vec<Preset>,
        selected: usize,
        audio_notice: Option<String>,
        show_codecs: bool,
        show_logs: bool,
        diagnostics: Arc<Diagnostics>,
        sessions: bridge_core::sessions::HostStore,
        selected_session: Option<String>,
        storage_snapshot: String,
        power_notice: String,
        show_sessions: bool,
    }

    fn set_message(state: &Arc<Mutex<State>>, message: impl Into<String>) {
        let message = message.into();
        if let Ok(mut state) = state.lock() {
            state.diagnostics.record("host", &message);
            state.message = message;
        }
    }

    fn set_screen_message(state: &Arc<Mutex<State>>, screen_id: ScreenId, message: &str) {
        if let Ok(mut state) = state.lock() {
            if let Some(screen) = state
                .screens
                .iter_mut()
                .find(|screen| screen.screen_id == screen_id)
            {
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

    fn sync_selected_settings(state: &mut State) -> Option<(ScreenId, VideoSettings)> {
        let screen_id = state.navigation.selected?;
        let settings = state.settings.clone();
        let screen = state
            .screens
            .iter_mut()
            .find(|screen| screen.screen_id == screen_id)?;
        if screen.removing {
            return None;
        }

        screen.status = if state.session_commands.is_some() {
            "Applying updated settings…"
        } else {
            "Prepared · waiting for a viewer"
        }
        .into();
        Some((screen_id, settings))
    }

    fn add_host_screen(state: &mut State) -> Option<ScreenId> {
        if state.screens.len() >= MAX_SCREENS {
            state.message = format!("A maximum of {MAX_SCREENS} screens is supported");
            return None;
        }
        let screen_id = state.next_screen_id;
        state.next_screen_id = state.next_screen_id.saturating_add(1);
        let name = format!("Screen {screen_id}");
        let diagnostics = Arc::new(Diagnostics::new(&format!("host-screen-{screen_id}")));
        let pin = match random_pin() {
            Ok(pin) => pin,
            Err(error) => {
                state.message = format!("Cannot generate screen PIN: {error}");
                return None;
            }
        };
        state.screens.push(HostScreen {
            display_id: None,
            display_position: None,
            pin,
            viewers: Vec::new(),
            stats_peer: None,
            screen_id,
            name: name.clone(),
            revision: 0,
            generation: 0,
            settings: state.settings.clone(),
            codec: None,
            status: "Prepared · waiting for a viewer".into(),
            removing: false,
            frames: 0,
            dropped: 0,
            bytes: 0,
            diagnostics,
            telemetry: Telemetry::new(),
            stats_view: StatsView::default(),
            viewer_stats: false,
        });
        if let Some(session) = state
            .sessions
            .sessions
            .iter_mut()
            .find(|s| Some(&s.contract.session_id) == state.selected_session.as_ref())
        {
            session
                .contract
                .screens
                .push(bridge_core::sessions::SavedScreen {
                    position: None,
                    screen_id,
                    name: name.clone(),
                    settings: state.settings.clone(),
                });
            session.contract.revision += 1;
        }
        state.navigation.open(screen_id);
        state.show_codecs = false;
        state.show_logs = false;
        if let Some(commands) = &state.session_commands {
            let _ = commands.try_send(HostUiCommand::AddScreen(screen_id));
        }
        state.message = format!("{name} prepared");
        Some(screen_id)
    }

    struct HostView {
        state: Arc<Mutex<State>>,
        account_inputs: [gpui_kit::Entity<InputState>; 2],
    }
    fn persist_sessions(state: &mut State) -> Result<()> {
        state.sessions.next_screen_id = state.next_screen_id;
        for screen in &mut state.screens {
            if let Some(id) = screen.display_id {
                if let Some(position) = mac::display_position(id) {
                    screen.display_position = Some(position);
                }
            }
        }
        for session in &mut state.sessions.sessions {
            let previous = bridge_core::sessions::contract_snapshot(&session.contract)?;
            session.contract.screens.retain(|screen| {
                state
                    .screens
                    .iter()
                    .any(|s| s.screen_id == screen.screen_id && !s.removing)
            });
            for saved in &mut session.contract.screens {
                if let Some(screen) = state
                    .screens
                    .iter()
                    .find(|s| s.screen_id == saved.screen_id)
                {
                    saved.position = screen.display_position;
                    saved.name = screen.name.clone();
                    saved.settings = screen.settings.clone();
                }
            }
            if previous != bridge_core::sessions::contract_snapshot(&session.contract)? {
                session.contract.revision = session.contract.revision.saturating_add(1);
            }
        }
        let snapshot = serde_snapshot(&state.sessions)?;
        if snapshot != state.storage_snapshot {
            #[cfg(not(test))]
            state.sessions.save()?;
            state.storage_snapshot = snapshot;
        }
        Ok(())
    }
    fn serde_snapshot(s: &bridge_core::sessions::HostStore) -> Result<String> {
        Ok(bridge_core::sessions::snapshot(s)?)
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
        action: fn(&mut State),
        cx: &mut Context<HostView>,
    ) -> impl IntoElement {
        Button::new(id)
            .label(label)
            .primary()
            .small()
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Ok(mut state) = shared.lock() {
                    if state.screens.iter().any(|screen| {
                        Some(screen.screen_id) == state.navigation.selected && screen.removing
                    }) {
                        return;
                    }
                    action(&mut state);
                    let update = if label == "Save custom preset" {
                        None
                    } else {
                        sync_selected_settings(&mut state)
                    };
                    if label != "Save custom preset" {
                        state.message = if state.session_commands.is_some() {
                            format!("{label} updated · applying to this screen")
                        } else {
                            format!("{label} updated · saved for the next connection")
                        };
                    }
                    state.diagnostics.record("settings", &state.message);
                    if let (Some((screen_id, settings)), Some(commands)) =
                        (update, state.session_commands.as_ref())
                    {
                        let _ =
                            commands.try_send(HostUiCommand::ChangeSettings(screen_id, settings));
                    }
                }
                cx.notify();
            }))
    }

    impl HostView {
        fn refresh(&mut self, cx: &mut Context<Self>) {
            if let Ok(mut state) = self.state.lock() {
                if let Err(error) = persist_sessions(&mut state) {
                    state.message = format!("Session save failed: {error}");
                }
                for s in &mut state.screens {
                    normalize_stats_target(s);
                    if s.stats_view.open {
                        let telemetry = s
                            .viewers
                            .iter()
                            .find(|p| Some(p.id) == s.stats_peer)
                            .map(|p| p.telemetry.clone())
                            .unwrap_or_else(|| s.telemetry.clone());
                        s.stats_view.refresh(&telemetry);
                    }
                }
            }
            cx.notify();
        }
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
    fn screen_name_for_inspection(screen: &HostScreen) -> String {
        format!(
            "{} · {}",
            screen.name,
            screen
                .viewers
                .iter()
                .find(|p| Some(p.id) == screen.stats_peer)
                .map(|p| p.name.as_str())
                .unwrap_or("shared source")
        )
    }
    impl HostView {
        fn inspect_stats(&mut self, id: ScreenId, cx: &mut Context<Self>) {
            if let Ok(mut state) = self.state.lock() {
                let Some(index) = state.screens.iter().position(|s| s.screen_id == id) else {
                    return;
                };
                if !state.screens[index].stats_view.open {
                    if let Some(Err(error)) = state
                        .session_commands
                        .as_ref()
                        .map(|commands| commands.try_send(HostUiCommand::Diagnostics(id, true)))
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

            let source = bridge_ui::InspectionSource::new(
                move || {
                    let mut state = read.lock().ok()?;
                    let screen = state.screens.iter_mut().find(|s| s.screen_id == id)?;
                    normalize_stats_target(screen);
                    if !screen.stats_view.open {
                        return None;
                    }
                    let telemetry = stats_telemetry(screen);
                    screen.stats_view.refresh(&telemetry);
                    let title = format!(
                        "Screen {id} · {} · generation {} · viewer {}",
                        screen_name_for_inspection(screen),
                        screen.generation,
                        screen
                            .stats_peer
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "shared source".into())
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
                        if let Some(screen) = state.screens.iter_mut().find(|s| s.screen_id == id) {
                            if !screen.stats_view.open {
                                return;
                            }
                            let telemetry = stats_telemetry(screen);
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
                                state.session_commands.as_ref().map(|commands| {
                                    commands.try_send(HostUiCommand::Diagnostics(id, false))
                                })
                            {
                                state.message = format!("Could not disable statistics: {error}");
                                state.diagnostics.record("stats", &state.message);
                            }
                        }
                        if matches!(action, bridge_ui::InspectionAction::Burst) {
                            let _ = state
                                .session_commands
                                .as_ref()
                                .map(|commands| commands.try_send(HostUiCommand::Burst(id)));
                        }
                    }
                },
            );
            if let Err(error) = bridge_ui::open_inspector(format!("host:{id}"), source.clone(), cx)
            {
                source.close();
                set_message(
                    &self.state,
                    format!("Cannot open statistics inspector: {error:#}"),
                );
            }
        }
    }

    impl Render for HostView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let state = self.state.lock().unwrap().clone();
            let mut selected_screen = state
                .screens
                .iter()
                .find(|screen| Some(screen.screen_id) == state.navigation.selected)
                .cloned();
            if let Some(screen) = &mut selected_screen {
                if let Some(peer) = screen
                    .viewers
                    .iter()
                    .find(|p| Some(p.id) == screen.stats_peer)
                {
                    screen.telemetry = peer.telemetry.clone();
                }
            }
            let screen_cards = state
                .screens
                .iter()
                .map(|screen| {
                    let shared = self.state.clone();
                    let screen_id = screen.screen_id;
                    let selected = Some(screen_id) == state.navigation.selected;
                    Button::new(("host-screen", screen_id as usize))
                        .w_full()
                        .min_w_0()
                        .label(format!(
                            "{}{} · {}",
                            if selected { "● " } else { "" },
                            screen.name,
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
                                if let Some((settings, name)) =
                                    state.screens.iter().find(|s| s.screen_id == screen_id).map(
                                        |screen| (screen.settings.clone(), screen.name.clone()),
                                    )
                                {
                                    state.settings = settings;
                                    state.message = format!("Selected {name}");
                                    state.navigation.view = ScreenView::Details;
                                }
                            }
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>();
            let add_screen = {
                let shared = self.state.clone();
                Button::new("host-add-screen")
                    .label("+ Add screen")
                    .primary()
                    .small()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Ok(mut state) = shared.lock() {
                            if let Some(screen_id) = add_host_screen(&mut state) {
                                state.navigation.view = ScreenView::Details;
                                state
                                    .diagnostics
                                    .record("screen", format!("Prepared screen {screen_id}"));
                            }
                        }
                        cx.notify();
                    }))
            };
            let presets = state
                .presets
                .iter()
                .enumerate()
                .map(|(index, preset)| {
                    let shared = self.state.clone();
                    let label = if index == state.selected {
                        format!("● {}", preset.name)
                    } else {
                        preset.name.clone()
                    };
                    Button::new(gpui_kit::SharedString::from(format!(
                        "preset:{}",
                        preset.name
                    )))
                    .label(label)
                    .outline()
                    .small()
                    .selected(index == state.selected)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Ok(mut state) = shared.lock() {
                            if state.screens.iter().any(|screen| {
                                Some(screen.screen_id) == state.navigation.selected
                                    && screen.removing
                            }) {
                                return;
                            }
                            state.selected = index;
                            state.settings = state.presets[index].settings.clone();
                            let update = sync_selected_settings(&mut state);
                            state.message = "Preset selected · applying to this screen".into();
                            state.diagnostics.record(
                                "settings",
                                format!("Selected {} preset", state.presets[index].name),
                            );
                            if let (Some((screen_id, settings)), Some(commands)) =
                                (update, state.session_commands.as_ref())
                            {
                                let _ = commands
                                    .try_send(HostUiCommand::ChangeSettings(screen_id, settings));
                            }
                        }
                        cx.notify();
                    }))
                })
                .collect::<Vec<_>>();
            let codec_panel = if state.show_codecs {
                let shared = self.state.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .child("Codec settings")
                    .child("H.264 · VideoToolbox hardware preferred, software available")
                    .child("AV1 appears when a working encoder is available.")
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
                                |s| std::mem::swap(&mut s.settings.width, &mut s.settings.height),
                                cx,
                            ))
                            .child(setting_button(
                                "FPS −",
                                "fps-minus",
                                shared.clone(),
                                |s| s.settings.fps = s.settings.fps.saturating_sub(5).max(5),
                                cx,
                            ))
                            .child(setting_button(
                                "FPS +",
                                "fps-plus",
                                shared.clone(),
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
                                |s| s.settings.cycle_codec_preference(),
                                cx,
                            ))
                            .child(setting_button(
                                "Cycle H.264 profile",
                                "profile",
                                shared.clone(),
                                |s| s.settings.h264_profile = s.settings.h264_profile.next(),
                                cx,
                            ))
                            .child(setting_button(
                                "Toggle low latency",
                                "low-latency",
                                shared.clone(),
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
                                |s| s.settings.vsync = !s.settings.vsync,
                                cx,
                            ))
                            .child(setting_button(
                                "Save custom preset",
                                "save-preset",
                                shared,
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
            let stats_panel = if let Some(screen) = &selected_screen {
                let id = screen.screen_id;
                div().id("host-stats-panel").child(
                    Button::new("host-inspect-stats")
                        .label("Open statistics")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.inspect_stats(id, cx))),
                )
            } else {
                div().id("host-stats-none")
            };
            let logs = if state.show_logs {
                div()
                    .id("host-logs-scroll")
                    .h(px(130.0))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(cx.theme().muted)
                    .text_sm()
                    .child(format!(
                        "File: {}",
                        selected_screen
                            .as_ref()
                            .map(|screen| screen.diagnostics.clone())
                            .unwrap_or_else(|| state.diagnostics.clone())
                            .path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unavailable".into())
                    ))
                    .children(
                        selected_screen
                            .as_ref()
                            .map(|screen| screen.diagnostics.clone())
                            .unwrap_or_else(|| state.diagnostics.clone())
                            .recent()
                            .into_iter()
                            .map(|line| div().child(line)),
                    )
            } else {
                div().id("host-logs-empty")
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
            let list_toggle = Button::new("host-screen-view-toggle-list")
                .label("Selected screen details →")
                .primary()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.view = ScreenView::Details;
                    }
                    cx.notify();
                }));
            let details_toggle = Button::new("host-screen-view-toggle-details")
                .label("← Screen list")
                .primary()
                .small()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.back();
                        state.show_codecs = false;
                        state.show_logs = false;
                    }
                    cx.notify();
                }));
            let screen_list_panel =
                if state.navigation.view == ScreenView::Details && selected_screen.is_some() {
                    div().id("host-screen-list-hidden")
                } else {
                    div()
                        .id("host-screen-list")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("SCREENS"),
                        )
                        .child(div().flex().flex_col().gap_1().children(screen_cards))
                        .child(add_screen)
                        .when(selected_screen.is_some(), |this| this.child(list_toggle))
                };
            let screen_details = if state.navigation.view == ScreenView::Details
                && selected_screen.is_some()
            {
                div()
                    .id("host-screen-details")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(details_toggle)
                    .child(selected_screen.as_ref().map(|screen|{if let Some(session)=state.sessions.sessions.iter().find(|session|session.contract.screens.iter().any(|s|s.screen_id==screen.screen_id)){format!("SESSION ACCOUNT · {}",session.contract.username)}else{format!("SCREEN PIN · {}",screen.pin)}}).unwrap_or_default())
                    .child(Button::new("capture-permission").label("Screen Recording access").outline().small().on_click(cx.listener(|this,_,_,cx|{let granted=mac::request_capture_access();set_message(&this.state,if granted{"Screen Recording is granted · retry this screen"}else{"Grant Screen Recording access to the launched app/executable, restart it and retry"});cx.notify();})))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .when(
                                state.session_commands.is_some()
                                    && selected_screen
                                        .as_ref()
                                        .is_some_and(|screen| !screen.removing),
                                |this| {
                                    this.child(
                                        Button::new("host-restart-screen").label("Restart stream").primary().small().on_click(cx.listener(|this, _, _, cx| {
                                                if let Ok(state) = this.state.lock() {
                                                    if let (Some(screen_id), Some(commands)) = (
                                                        state.navigation.selected,
                                                        &state.session_commands,
                                                    ) {
                                                        let _ = commands.try_send(
                                                            HostUiCommand::ChangeSettings(
                                                                screen_id,
                                                                state.settings.clone(),
                                                            ),
                                                        );
                                                    }
                                                }
                                                cx.notify();
                                            })),
                                    )
                                },
                            )
                            .when(
                                selected_screen
                                    .as_ref()
                                    .is_some_and(|screen| !screen.removing),
                                |panel| {
                                    panel.child(
                                        Button::new("host-remove-screen").label("Remove screen").danger().small().on_click(cx.listener(|this, _, _, cx| {
                                                let selected =
                                                    this.state.lock().ok().and_then(|state| {
                                                        state.navigation.selected.map(|id| {
                                                            (id, state.session_commands.clone())
                                                        })
                                                    });
                                                if let Some((id, commands)) = selected {
                                                    if let Some(commands) = commands {
                                                        let _ = commands
                                                            .try_send(HostUiCommand::RemoveScreen(id));
                                                    } else {
                                                        remove_host_screen(&this.state, id);
                                                    }
                                                }
                                                cx.notify();
                                            })),
                                    )
                                },
                            ),
                    )
                    .child(
                        selected_screen
                            .as_ref()
                            .map(|screen| {
                                format!("Stream details · {} · {}", screen.name, screen.status)
                            })
                            .unwrap_or_else(|| "No screen selected".into()),
                    )
                    .child(format!(
                        "{}×{} · {} fps · video only",
                        state.settings.width, state.settings.height, state.settings.fps
                    ))
                    .child(logs_toggle)
                    .child(logs)
                    .when(
                        selected_screen
                            .as_ref()
                            .is_some_and(|screen| !screen.removing),
                        |panel| {
                            panel
                                .child(div().flex().flex_wrap().gap_2().children(presets))
                                .child(
                                    Button::new("codec-menu").label(if state.show_codecs {
                                            "Settings ▴"
                                        } else {
                                            "Settings ▾"
                                        }).primary().small().on_click(cx.listener(|this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                state.show_codecs = !state.show_codecs;
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(codec_panel)
                                .child(stats_panel)
                                .child("Connected viewers")
                                .children(selected_screen.as_ref().map(|s|s.viewers.clone()).unwrap_or_default().into_iter().map(|peer|{
                                    let id=peer.id;let screen_id=selected_screen.as_ref().map(|s|s.screen_id).unwrap_or(0);let selected=selected_screen.as_ref().is_some_and(|s|s.stats_peer==Some(id));
                                    div().id(("connected-viewer",id as usize)).p_2().rounded_md().bg(if selected { cx.theme().accent } else { cx.theme().secondary }).flex().justify_between()
                                      .child(Button::new(("peer-stats",id as usize)).label(format!("{} · {} · select stats",peer.name,peer.status)).outline().small().selected(selected).flex_1().min_w_0().on_click(cx.listener(move|this,_,_,cx|{if let Ok(mut state)=this.state.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.screen_id==screen_id){s.stats_peer=Some(id);s.stats_view.reset();}}cx.notify();})))
                                      .child(Button::new(("disconnect-peer",id as usize)).label("Disconnect").outline().small().on_click(cx.listener(move|this,_,_,cx|{if let Ok(state)=this.state.lock(){if let Some(commands)=&state.session_commands{let _=commands.try_send(HostUiCommand::DisconnectPeer(screen_id,id));}}cx.notify();})))
                                }))
                        },
                    )
                    .child(
                        div()
                            .rounded_md()
                            .p_2()
                            .bg(cx.theme().secondary)
                            .child(
                                state
                                    .audio_notice
                                    .clone()
                                    .unwrap_or_else(|| "Audio off · video only".into()),
                            ),
                    )
                    .child(
                        div()
                            .rounded_md()
                            .p_2()
                            .bg(cx.theme().secondary)
                            .child(format!("Status: {}", state.message)),
                    )
            } else {
                div().id("host-screen-details-hidden")
            };
            div()
                .size_full()
                .min_h_0()
                .on_mouse_down(MouseButton::Left, |event, window, _| {
                    resize_from_border(event, window)
                })
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .text_color(cx.theme().foreground)
                .flex()
                .flex_col()
                .child(TitleBar::new().child("AirBridge · Host"))
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .bg(cx.theme().secondary)
                        .child(state.message.clone()),
                )
                .child(
                    div()
                        .id("host-settings-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(Button::new("sessions-menu").label(if state.show_sessions{"Sessions ▴"}else{"Sessions ▾"}).outline().small().on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.show_sessions=!s.show_sessions;}cx.notify();})))
                        .when(state.show_sessions,|panel|panel.child(div().flex().flex_col().gap_2().child("Saved sessions")
                            .children(state.sessions.sessions.iter().map(|session|{
                                let id=session.contract.session_id.clone();let selected=state.selected_session.as_ref()==Some(&id);
                                Button::new(gpui_kit::SharedString::from(format!("session-select-{id}"))).label(format!("{} · {} screens",session.contract.username,session.contract.screens.len())).outline().small().selected(selected).on_click(cx.listener(move|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){s.selected_session=Some(id.clone());s.message="New screens will belong to this session".into();}cx.notify();}))
                            }))
                            .child(Form::new().small().child(Field::new().label("Account").child(Input::new(&self.account_inputs[0]).small())).child(Field::new().label("Session password").child(Input::new(&self.account_inputs[1]).small().mask_toggle())))
                            .child(Button::new("session-create").label("Save current screens as session").primary().small().on_click(cx.listener(|this,_,window,cx|{
                                let user=this.account_inputs[0].read(cx).value().to_string();let password=bridge_core::sessions::Secret(this.account_inputs[1].read(cx).value().to_string());
                                this.account_inputs[1].update(cx,|input,cx|input.set_value("",window,cx));
                                let shared=this.state.clone();std::thread::spawn(move||{
                                    let snapshot=shared.lock().ok().map(|s|(s.sessions.clone(),s.screens.iter().filter(|screen|!screen.removing&&!s.sessions.sessions.iter().any(|session|session.contract.screens.iter().any(|saved|saved.screen_id==screen.screen_id))).map(|screen|bridge_core::sessions::SavedScreen{position:screen.display_position,screen_id:screen.screen_id,name:screen.name.clone(),settings:screen.settings.clone()}).collect::<Vec<_>>()));
                                    if let Some((mut store,screens))=snapshot {let result=store.create(user,&password.0,screens);if let Ok(mut s)=shared.lock(){match result{Ok(id)=>{
                                        let Some(session)=store.sessions.pop() else{return;};
                                        if s.sessions.sessions.iter().any(|old|old.contract.username==session.contract.username || old.contract.screens.iter().any(|a|session.contract.screens.iter().any(|b|a.screen_id==b.screen_id))){s.message="Session creation conflicted with another change; retry".into();return;}
                                        s.sessions.next_screen_id=s.sessions.next_screen_id.max(s.next_screen_id);
                                        s.sessions.sessions.push(session);match s.sessions.save(){Ok(())=>{s.selected_session=Some(id);s.message="Session saved · login from the viewer using its account and password".into();},Err(e)=>{s.sessions.sessions.pop();s.message=format!("Session save failed: {e}");}}
                                    },Err(e)=>s.message=format!("Session creation failed: {e}")}}}
                                });cx.notify();
                            })))
                            .when(state.selected_session.is_some(),|panel|panel.child(Button::new("host-keep-awake").label(if state.sessions.sessions.iter().any(|s|Some(&s.contract.session_id)==state.selected_session.as_ref()&&s.contract.keep_awake){"Keep host awake: on"}else{"Keep host awake: off"}).outline().small().on_click(cx.listener(|this,_,_,cx|{if let Ok(mut s)=this.state.lock(){let id=s.selected_session.clone();if let Some(session)=s.sessions.sessions.iter_mut().find(|session|Some(&session.contract.session_id)==id.as_ref()){session.contract.keep_awake=!session.contract.keep_awake;session.contract.revision+=1;}}cx.notify();}))))
                            .child(state.power_notice.clone())))
                        .child(screen_list_panel)
                        .child(screen_details),
                )
        }
    }

    fn random_pin() -> Result<String> {
        use std::io::Read;
        let mut random = [0; 4];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        Ok(format!("{:06}", u32::from_le_bytes(random) % 1_000_000))
    }

    fn remove_host_screen(shared: &Arc<Mutex<State>>, screen_id: ScreenId) {
        if let Ok(mut state) = shared.lock() {
            state.screens.retain(|screen| screen.screen_id != screen_id);
            let fallback = state.screens.first().map(|screen| screen.screen_id);
            if state.navigation.selected == Some(screen_id) {
                state.show_codecs = false;
                state.show_logs = false;
            }
            state.navigation.remove(screen_id, fallback);
            if let Some(settings) = state
                .screens
                .iter()
                .find(|screen| Some(screen.screen_id) == state.navigation.selected)
                .map(|screen| screen.settings.clone())
            {
                state.settings = settings;
            }
            state.message = format!("Screen {screen_id} removed");
            state.diagnostics.record("screen", &state.message);
        }
    }

    fn initial_state(presets: Vec<Preset>, pin: String) -> State {
        State {
            message: "Starting…".into(),
            settings: presets[1].settings.clone(),
            screens: vec![HostScreen {
                display_id: None,
                display_position: None,
                pin: pin.clone(),
                viewers: Vec::new(),
                stats_peer: None,
                screen_id: 1,
                name: "Screen 1".into(),
                revision: 0,
                generation: 0,
                settings: presets[1].settings.clone(),
                codec: None,
                status: "Prepared · waiting for a viewer".into(),
                removing: false,
                frames: 0,
                dropped: 0,
                bytes: 0,
                diagnostics: Arc::new(Diagnostics::new("host-screen-1")),
                telemetry: Telemetry::new(),
                stats_view: StatsView::default(),
                viewer_stats: false,
            }],
            navigation: ScreenNavigation {
                selected: Some(1),
                view: ScreenView::List,
            },
            next_screen_id: 2,
            session_commands: None,
            presets,
            selected: 1,
            audio_notice: None,
            show_codecs: false,
            show_logs: false,
            diagnostics: Arc::new(Diagnostics::new("host")),
            sessions: bridge_core::sessions::HostStore::new()
                .expect("System randomness unavailable"),
            selected_session: None,
            storage_snapshot: String::new(),
            power_notice: String::new(),
            show_sessions: false,
        }
    }

    pub fn main() {
        let mut presets = built_in_presets();
        if let Ok(custom) = load_custom_presets() {
            presets.extend(custom);
        }
        let pin = random_pin().expect("System randomness is unavailable");
        let mut initial = initial_state(presets, pin);
        match bridge_core::sessions::HostStore::load() {
            Ok(store) => {
                let saved = store
                    .sessions
                    .iter()
                    .flat_map(|s| s.contract.screens.clone())
                    .collect::<Vec<_>>();
                if !saved.is_empty() {
                    initial.screens.clear();
                    for screen in saved {
                        if initial
                            .screens
                            .iter()
                            .any(|s| s.screen_id == screen.screen_id)
                        {
                            continue;
                        }
                        let id = screen.screen_id;
                        initial.screens.push(HostScreen {
                            display_id: None,
                            display_position: screen.position,
                            pin: random_pin().expect("System randomness unavailable"),
                            viewers: vec![],
                            stats_peer: None,
                            screen_id: id,
                            name: screen.name,
                            revision: 0,
                            generation: 0,
                            settings: screen.settings,
                            codec: None,
                            status: "Prepared · restored session".into(),
                            removing: false,
                            frames: 0,
                            dropped: 0,
                            bytes: 0,
                            diagnostics: Arc::new(Diagnostics::new(&format!("host-screen-{id}"))),
                            telemetry: Telemetry::new(),
                            stats_view: StatsView::default(),
                            viewer_stats: false,
                        });
                    }
                    initial.next_screen_id = initial
                        .screens
                        .iter()
                        .map(|s| s.screen_id)
                        .max()
                        .unwrap_or(0)
                        + 1;
                    initial.navigation.selected = initial.screens.first().map(|s| s.screen_id);
                }
                initial.next_screen_id = initial.next_screen_id.max(store.next_screen_id);
                initial.selected_session = store
                    .sessions
                    .first()
                    .map(|s| s.contract.session_id.clone());
                initial.sessions = store;
            }
            Err(error) => {
                eprintln!("Cannot load host sessions: {error}");
                return;
            }
        }
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
                set_message(&server_state, format!("Server could not start: {error:#}"));
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
                let widget_size = size(px(390.0), px(470.0));
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
                        window_min_size: Some(size(px(330.0), px(280.0))),
                        ..TitleBar::window_options()
                    },
                    cx,
                    |window, cx| {
                        let view = cx.new(|cx| HostView {
                            state: state.clone(),
                            account_inputs: [
                                cx.new(|cx| InputState::new(window, cx).placeholder("Username")),
                                cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("Password (8+ bytes)")
                                        .masked(true)
                                }),
                            ],
                        });
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

        #[gpui_kit::test]
        fn statistics_live_only_in_inspector_and_close_releases_subscription(
            cx: &mut gpui_kit::TestAppContext,
        ) {
            use gpui_kit::test::TestWindowExt;
            let (commands, mut requests) = mpsc::channel(16);
            let mut initial = initial_state(built_in_presets(), "123456".into());
            initial.navigation.open(1);
            initial.session_commands = Some(commands);
            let state = Arc::new(Mutex::new(initial));
            cx.update(gpui_kit::init);
            let (controls, _) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| HostView {
                        account_inputs: [
                            cx.new(|cx| InputState::new(window, cx)),
                            cx.new(|cx| InputState::new(window, cx).masked(true)),
                        ],
                        state: state.clone(),
                    })
                })
                .unwrap()
            });
            controls
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    window.click("host-inspect-stats", cx);
                    window.click("host-inspect-stats", cx);
                    assert!(window.try_find("host-timing-table").is_none());
                    assert!(window.try_find("host-timing-frames").is_none());
                    window.click("host-screen-view-toggle-details", cx);
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
        fn kit_buttons_switch_list_details_and_screen_ownership(cx: &mut gpui_kit::TestAppContext) {
            use gpui_kit::test::TestWindowExt;
            let state = Arc::new(Mutex::new(initial_state(
                built_in_presets(),
                "123456".into(),
            )));
            cx.update(gpui_kit::init);
            let (handle, _) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| HostView {
                        account_inputs: [
                            cx.new(|cx| InputState::new(window, cx)),
                            cx.new(|cx| InputState::new(window, cx).masked(true)),
                        ],
                        state: state.clone(),
                    })
                })
                .unwrap()
            });
            handle
                .update(cx, |_, window, cx| {
                    window.render_frame(cx);
                    assert_eq!(
                        window.find(("host-screen", 1_usize)).role(),
                        Some(gpui_kit::Role::Button)
                    );
                    assert_eq!(state.lock().unwrap().navigation.selected, Some(1));
                    window.click(("host-screen", 1_usize), cx);
                    assert!(window.try_find("host-add-screen").is_none());
                    window.click("codec-menu", cx);
                })
                .unwrap();
            assert!(state.lock().unwrap().show_codecs);
            handle
                .update(cx, |_, window, cx| {
                    window.click("host-screen-view-toggle-details", cx);
                    assert!(window.try_find("codec-menu").is_none());
                    window.click("host-add-screen", cx);
                })
                .unwrap();
            assert_eq!(state.lock().unwrap().screens.len(), 2);
            assert_eq!(state.lock().unwrap().navigation.selected, Some(2));
            assert!(!state.lock().unwrap().show_codecs);
            handle
                .update(cx, |_, window, cx| window.click("host-remove-screen", cx))
                .unwrap();
            assert_eq!(state.lock().unwrap().screens.len(), 1);
            assert_eq!(state.lock().unwrap().navigation.view, ScreenView::List);
        }
        #[test]
        fn screen_pins_and_selection_are_independent() {
            let mut s = initial_state(built_in_presets(), "123456".into());
            assert_eq!(s.screens[0].pin, "123456");
            s.screens[0].stats_peer = Some(999);
            s.screens[0].stats_view.freeze_after = Some(20);
            normalize_stats_target(&mut s.screens[0]);
            assert!(s.screens[0].stats_peer.is_none());
            assert!(s.screens[0].stats_view.freeze_after.is_none());
            let id = add_host_screen(&mut s).unwrap();
            assert_eq!(s.navigation.selected, Some(id));
            assert_eq!(s.screens.len(), 2);
            assert_eq!(s.screens[1].pin.len(), 6);
            let shared = Arc::new(Mutex::new(s));
            remove_host_screen(&shared, 1);
            assert_eq!(shared.lock().unwrap().screens.len(), 1);
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    app::main();
}
