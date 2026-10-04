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
    use gpui::{
        App, Application, Bounds, Context, MouseButton, ResizeEdge, Window, WindowBounds,
        WindowDecorations, WindowOptions, bounds, div, point, prelude::*, px, rgb, size,
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
        action: fn(&mut State),
        cx: &mut Context<HostView>,
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

    impl Render for HostView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if let Ok(mut state) = self.state.lock() {
                for s in &mut state.screens {
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
                    div()
                        .id(("host-screen", screen_id as usize))
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
                            screen.name,
                            screen.status
                        ))
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
                div()
                    .id("host-add-screen")
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x35517a))
                    .cursor_pointer()
                    .child("+ Add screen")
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
                    div()
                        .id(("preset", index))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(if index == state.selected {
                            0x31658e
                        } else {
                            0x263248
                        }))
                        .border_1()
                        .border_color(rgb(if index == state.selected {
                            0x6fd3ff
                        } else {
                            0x344158
                        }))
                        .cursor_pointer()
                        .child(label)
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
                                    let _ = commands.try_send(HostUiCommand::ChangeSettings(
                                        screen_id, settings,
                                    ));
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
                    .bg(rgb(0x263248))
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
                        div()
                            .id("host-stats-toggle")
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
                                    div()
                                        .id("host-timing-frames")
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
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Frames;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("host-timing-averages")
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
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.mode = TimingView::Averages;
                                                }
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("host-timing-freeze")
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
                                                    .find(|s| s.screen_id == id)
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
                                        .id("host-timing-burst")
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
                                                    .find(|s| s.screen_id == id)
                                                {
                                                    s.stats_view.frozen = None;
                                                    s.stats_view.freeze_after = Some(
                                                        s.telemetry.snapshot().averages
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
                                .lines(&screen.telemetry)
                                .into_iter()
                                .map(|line| div().text_sm().child(line)),
                        )
                        .child(
                            div()
                                .id("host-timing-table")
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
                div().id("host-stats-none")
            };
            let logs = if state.show_logs {
                div()
                    .id("host-logs-scroll")
                    .h(px(130.0))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0x0d1420))
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
                .id("host-screen-view-toggle-list")
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x35517a))
                .cursor_pointer()
                .child("Selected screen details →")
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.navigation.view = ScreenView::Details;
                    }
                    cx.notify();
                }));
            let details_toggle = div()
                .id("host-screen-view-toggle-details")
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x35517a))
                .cursor_pointer()
                .child("← Screen list")
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
                        .child(div().text_sm().text_color(rgb(0xa8b8cf)).child("SCREENS"))
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
                    .child(div().id("capture-permission").p_2().rounded_md().bg(rgb(0x28354b)).cursor_pointer().child("Screen Recording access")
                        .on_click(cx.listener(|this,_,_,cx|{let granted=mac::request_capture_access();set_message(&this.state,if granted{"Screen Recording is granted · retry this screen"}else{"Grant Screen Recording access to the launched app/executable, restart it and retry"});cx.notify();})))
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
                                        div()
                                            .id("host-restart-screen")
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(0x35517a))
                                            .cursor_pointer()
                                            .child("Restart stream")
                                            .on_click(cx.listener(|this, _, _, cx| {
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
                                        div()
                                            .id("host-remove-screen")
                                            .px_3()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(0x573846))
                                            .cursor_pointer()
                                            .child("Remove screen")
                                            .on_click(cx.listener(|this, _, _, cx| {
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
                                    div()
                                        .id("codec-menu")
                                        .px_3()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(0x35517a))
                                        .cursor_pointer()
                                        .child(if state.show_codecs {
                                            "Settings ▴"
                                        } else {
                                            "Settings ▾"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
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
                                    div().id(("connected-viewer",id as usize)).p_2().rounded_md().bg(rgb(if selected{0x35517a}else{0x28354b})).flex().justify_between()
                                      .child(div().id(("peer-stats",id as usize)).cursor_pointer().child(format!("{} · {} · select stats",peer.name,peer.status)).on_click(cx.listener(move|this,_,_,cx|{if let Ok(mut state)=this.state.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.screen_id==screen_id){s.stats_peer=Some(id);s.stats_view.reset();}}cx.notify();})))
                                      .child(div().id(("disconnect-peer",id as usize)).px_2().cursor_pointer().child("Disconnect").on_click(cx.listener(move|this,_,_,cx|{if let Ok(state)=this.state.lock(){if let Some(commands)=&state.session_commands{let _=commands.try_send(HostUiCommand::DisconnectPeer(screen_id,id));}}cx.notify();})))
                                }))
                        },
                    )
                    .child(
                        div()
                            .rounded_md()
                            .p_2()
                            .bg(rgb(if state.audio_notice.is_some() {
                                0x4a3924
                            } else {
                                0x203149
                            }))
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
                            .bg(rgb(0x203149))
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
                .bg(rgb(0x141c2b))
                .border_1()
                .border_color(rgb(0x36465f))
                .text_color(rgb(0xf2f5fb))
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(42.0))
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .bg(rgb(0x1d2940))
                        .child(
                            div()
                                .flex_1()
                                .child("AIRBRIDGE  ·  HOST")
                                .on_mouse_down(MouseButton::Left, |_, window, _| {
                                    window.start_window_move()
                                }),
                        )
                        .child(
                            div()
                                .id("minimize")
                                .px_2()
                                .cursor_pointer()
                                .child("−")
                                .on_click(cx.listener(|_, _, window, _| window.minimize_window())),
                        )
                        .child(
                            div()
                                .id("close")
                                .px_2()
                                .cursor_pointer()
                                .child("×")
                                .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                        ),
                )
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .bg(rgb(0x203149))
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
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    window_decorations: Some(WindowDecorations::Client),
                    window_min_size: Some(size(px(330.0), px(280.0))),
                    ..Default::default()
                },
                |_, cx| {
                    let view = cx.new(|_| HostView {
                        state: state.clone(),
                    });
                    let view_weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(250))
                                .await;
                            if view_weak.update(cx, |_, cx| cx.notify()).is_err() {
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
        #[test]
        fn screen_pins_and_selection_are_independent() {
            let mut s = initial_state(built_in_presets(), "123456".into());
            assert_eq!(s.screens[0].pin, "123456");
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
