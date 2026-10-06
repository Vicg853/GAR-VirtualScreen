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
        frames: u64,
        dropped: u64,
        bytes: u64,
        audio_notice: Option<String>,
        show_codecs: bool,
        show_logs: bool,
        diagnostics: Arc<Diagnostics>,
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
        let ids = state
            .screens
            .iter_mut()
            .filter(|s| s.stats_view.open)
            .map(|s| {
                s.stats_view.open = false;
                s.screen_id
            })
            .collect::<Vec<_>>();
        if let Some(commands) = &state.session_commands {
            for id in ids {
                let _ = commands.try_send(HostUiCommand::Diagnostics(id, false));
            }
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
                        if matches!(action, bridge_ui::InspectionAction::Burst) {
                            let _ = state
                                .session_commands
                                .as_ref()
                                .map(|commands| commands.try_send(HostUiCommand::Burst(id)));
                        }
                    }
                },
            );
            if let Err(error) = bridge_ui::open_inspector(format!("host:{id}"), source, cx) {
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
                let mut panel = div()
                    .id("host-stats-panel")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        Button::new("host-stats-toggle")
                            .label(if screen.stats_view.open {
                                "Stats ▴ · instrumented stream"
                            } else {
                                "Stats ▾"
                            })
                            .outline()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Ok(mut state) = this.state.lock() {
                                    if let Some(s) =
                                        state.screens.iter_mut().find(|s| s.screen_id == id)
                                    {
                                        let enabled = !s.stats_view.open;
                                        s.stats_view.open = enabled;
                                        s.stats_view.frozen = None;
                                        if let Some(commands) = &state.session_commands {
                                            let _ = commands
                                                .try_send(HostUiCommand::Diagnostics(id, enabled));
                                        }
                                    }
                                }
                                cx.notify();
                            })),
                    );
                if screen.stats_view.open {
                    panel = panel
                        .child(
                            Button::new("host-inspect-stats")
                                .label("Open statistics inspector")
                                .primary()
                                .small()
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.inspect_stats(id, cx)),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(bridge_ui::TIMING_HELP),
                        )
                        .child(format!(
                            "Stats target: {}",
                            screen
                                .viewers
                                .iter()
                                .find(|p| Some(p.id) == screen.stats_peer)
                                .map(|p| p.name.clone())
                                .unwrap_or_else(|| "Shared source (select a viewer below)".into())
                        ))
                        .child("Temporary diagnostics · opening/closing restarts this stream")
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Button::new("host-timing-frames")
                                        .label("Frame rows")
                                        .outline()
                                        .small()
                                        .selected(screen.stats_view.mode == TimingView::Frames)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Frames;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("host-timing-averages")
                                        .label("Stage averages")
                                        .outline()
                                        .small()
                                        .selected(screen.stats_view.mode == TimingView::Averages)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Averages;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("host-timing-freeze")
                                        .label(if screen.stats_view.frozen.is_some() {
                                            "Resume"
                                        } else {
                                            "Freeze"
                                        })
                                        .outline()
                                        .small()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.frozen =
                                                        if s.stats_view.frozen.is_some() {
                                                            None
                                                        } else {
                                                            Some(stats_telemetry(s).snapshot())
                                                        };
                                                    s.stats_view.freeze_after = None;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("host-timing-burst")
                                        .label("Capture next 20")
                                        .outline()
                                        .small()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Ok(mut state) = this.state.lock() {
                                                if let Some(s) = state
                                                    .screens
                                                    .iter_mut()
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.frozen = None;
                                                    s.stats_view.freeze_after = Some(
                                                        stats_telemetry(s).snapshot().averages
                                                            [Stage::Capture as usize]
                                                            .count
                                                            + 20,
                                                    );
                                                }
                                                if let Some(commands) = &state.session_commands {
                                                    let _ =
                                                        commands.try_send(HostUiCommand::Burst(id));
                                                }
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                        .children(
                            screen
                                .stats_view
                                .lines(&stats_telemetry(screen))
                                .into_iter()
                                .map(|line| div().text_sm().child(line)),
                        )
                        .child(
                            div()
                                .id("host-timing-table")
                                .min_w_0()
                                .h(gpui_kit::rems(14.))
                                .overflow_scroll()
                                .child(bridge_ui::timing_preview(
                                    screen.stats_view.table(&stats_telemetry(screen)),
                                )),
                        );
                }
                panel
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
                        let ids = state
                            .screens
                            .iter_mut()
                            .filter(|s| s.stats_view.open)
                            .map(|s| {
                                s.stats_view.open = false;
                                s.screen_id
                            })
                            .collect::<Vec<_>>();
                        if let Some(commands) = &state.session_commands {
                            for id in ids {
                                let _ = commands.try_send(HostUiCommand::Diagnostics(id, false));
                            }
                        }
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
                    .child(selected_screen.as_ref().map(|screen|format!("SCREEN PIN · {}",screen.pin)).unwrap_or_default())
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
                    .child(format!(
                        "Sent {} frames · {} capture drops · {:.1} MB",
                        selected_screen
                            .as_ref()
                            .map(|screen| screen.frames)
                            .unwrap_or(state.frames),
                        selected_screen
                            .as_ref()
                            .map(|screen| screen.dropped)
                            .unwrap_or(state.dropped),
                        selected_screen
                            .as_ref()
                            .map(|screen| screen.bytes)
                            .unwrap_or(state.bytes) as f64
                            / 1_000_000.0
                    ))
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
            frames: 0,
            dropped: 0,
            bytes: 0,
            audio_notice: None,
            show_codecs: false,
            show_logs: false,
            diagnostics: Arc::new(Diagnostics::new("host")),
        }
    }

    pub fn main() {
        let mut presets = built_in_presets();
        if let Ok(custom) = load_custom_presets() {
            presets.extend(custom);
        }
        let pin = random_pin().expect("System randomness is unavailable");
        let state = Arc::new(Mutex::new(initial_state(presets, pin)));
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
                        let view = cx.new(|_| HostView {
                            state: state.clone(),
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
        fn kit_buttons_switch_list_details_and_screen_ownership(cx: &mut gpui_kit::TestAppContext) {
            use gpui_kit::test::TestWindowExt;
            let state = Arc::new(Mutex::new(initial_state(
                built_in_presets(),
                "123456".into(),
            )));
            cx.update(gpui_kit::init);
            let (handle, _) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                    cx.new(|_| HostView {
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
