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
            CodecOption, Preset, VideoSettings, built_in_presets, load_custom_presets, negotiate,
            save_custom_presets,
        },
        diagnostics::Diagnostics,
        discovery::Advertiser,
        lifecycle::{ScreenNavigation, ScreenView},
        protocol::{
            AddScreenRequest, ChangeScreenSettingsRequest, ClientHello, ClientMessage,
            ConfigRevision, HostMessage, MAX_SCREENS, MediaLane, MediaState, MediaStreamHello,
            PORT, RequestId, ScreenConfig, ScreenId, StreamGeneration, VERSION, control_reader,
            receive_json, send_control, send_debug_media, send_json, send_media,
        },
        security::{authenticate_host, server_endpoint},
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
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot, watch};

    #[derive(Clone)]
    struct HostScreen {
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

    impl HostScreen {
        fn instrumented(&self) -> bool {
            self.stats_view.open || self.viewer_stats
        }
    }
    fn ingest_diagnostic_batch(
        shared: &Arc<Mutex<State>>,
        batch: bridge_core::telemetry::DiagnosticBatch,
    ) {
        if let Ok(state) = shared.lock() {
            if let Some(screen) = state.screens.iter().find(|s| {
                s.screen_id == batch.screen_id
                    && s.generation == batch.generation
                    && s.instrumented()
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

    enum HostUiCommand {
        AddScreen(ScreenId),
        ChangeSettings(ScreenId, VideoSettings),
        RemoveScreen(ScreenId),
        Diagnostics(ScreenId, bool),
        Burst(ScreenId),
        Disconnect,
    }

    #[derive(Clone)]
    struct State {
        pin: String,
        viewer_name: Option<String>,
        selected_stats_viewer: Option<String>,
        message: String,
        settings: VideoSettings,
        screens: Vec<HostScreen>,
        navigation: ScreenNavigation,
        next_screen_id: ScreenId,
        session_commands: Option<mpsc::UnboundedSender<HostUiCommand>>,
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
        screen.settings = settings.clone();
        screen.revision = screen.revision.saturating_add(1);
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
        state.screens.push(HostScreen {
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
                let _ = commands.send(HostUiCommand::Diagnostics(id, false));
            }
        }
        state.navigation.open(screen_id);
        state.show_codecs = false;
        state.show_logs = false;
        if let Some(commands) = &state.session_commands {
            let _ = commands.send(HostUiCommand::AddScreen(screen_id));
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
                        let _ = commands.send(HostUiCommand::ChangeSettings(screen_id, settings));
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
                        s.stats_view.refresh(&s.telemetry);
                    }
                }
            }
            let state = self.state.lock().unwrap().clone();
            let selected_screen = state
                .screens
                .iter()
                .find(|screen| Some(screen.screen_id) == state.navigation.selected)
                .cloned();
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
                                    let _ = commands
                                        .send(HostUiCommand::ChangeSettings(screen_id, settings));
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
                                                .send(HostUiCommand::Diagnostics(id, enabled));
                                        }
                                    }
                                }
                                cx.notify();
                            })),
                    );
                if screen.stats_view.open {
                    panel = panel
                        .child(
                            div()
                                .id("stats-viewer-choice")
                                .px_2()
                                .py_1()
                                .bg(rgb(if state.selected_stats_viewer == state.viewer_name {
                                    0x35517a
                                } else {
                                    0x28354b
                                }))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Ok(mut state) = this.state.lock() {
                                        state.selected_stats_viewer = state.viewer_name.clone();
                                    }
                                    cx.notify();
                                }))
                                .child(format!(
                                    "Selected viewer: {}",
                                    state
                                        .viewer_name
                                        .clone()
                                        .unwrap_or_else(|| "None connected".into())
                                )),
                        )
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
                                                    let _ = commands.send(HostUiCommand::Burst(id));
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
                                let _ = commands.send(HostUiCommand::Diagnostics(id, false));
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
                                                        let _ = commands.send(
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
                                                            .send(HostUiCommand::RemoveScreen(id));
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
                .when(state.session_commands.is_some(), |this| {
                    this.child(
                        div()
                            .id("host-disconnect")
                            .mx_3()
                            .my_2()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x573846))
                            .cursor_pointer()
                            .child("Disconnect viewer")
                            .on_click(cx.listener(|this, _, _, _| {
                                if let Ok(state) = this.state.lock() {
                                    if let Some(commands) = &state.session_commands {
                                        let _ = commands.send(HostUiCommand::Disconnect);
                                    }
                                }
                            })),
                    )
                })
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
                        .when(state.session_commands.is_none(), |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xa8b8cf))
                                    .child("PAIRING PIN"),
                            )
                            .child(div().text_xl().child(state.pin.clone()))
                        })
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

    fn run_server(shared: Arc<Mutex<State>>, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let result = runtime.block_on(async move {
            let endpoint =
                server_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT))?;
            let _advertiser = Advertiser::new("AirBridge Mac")?;
            set_message(&shared, "Ready for a Linux viewer");
            loop {
                let incoming = tokio::select! {
                    _ = shutdown.changed() => break,
                    incoming = endpoint.accept() => { let Some(incoming) = incoming else { break; }; incoming }
                };
                let accepted = tokio::select! { _ = shutdown.changed() => break, accepted = incoming => accepted };
                let connection = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        set_message(&shared, format!("Connection failed: {error}"));
                        continue;
                    }
                };
                let result = handle_connection(connection.clone(), shared.clone(), shutdown.clone()).await;
                match result {
                    Ok(()) => set_message(&shared, "Viewer disconnected. Ready to reconnect."),
                    Err(error) => set_message(&shared, format!("Connection error: {error:#}")),
                }
                connection.close(0u32.into(), b"session ended");
                if *shutdown.borrow() { break; }
            }
            endpoint.close(0u32.into(), b"host shutdown");
            Ok(())
        });
        runtime.shutdown_timeout(Duration::from_secs(2));
        result
    }

    enum HostSessionEvent {
        Wire(HostMessage),
        RestartFinished {
            screen_id: ScreenId,
            request_id: Option<RequestId>,
            kind: RestartKind,
            result: std::result::Result<ScreenConfig, String>,
        },
        RemoveFinished {
            screen_id: ScreenId,
            request_id: Option<RequestId>,
            result: std::result::Result<(), String>,
        },
        Finished {
            screen_id: ScreenId,
            generation: StreamGeneration,
            error: Option<String>,
        },
    }

    struct RunningStream {
        generation: StreamGeneration,
        stop: Arc<AtomicBool>,
        cancel: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for RunningStream {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            self.task.abort();
        }
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

    #[allow(clippy::too_many_arguments)]
    fn schedule_screen_removal(
        screen_id: ScreenId,
        request_id: Option<RequestId>,
        streams: &mut std::collections::HashMap<ScreenId, RunningStream>,
        restarting: &mut std::collections::HashSet<ScreenId>,
        removing: &mut std::collections::HashMap<ScreenId, Option<RequestId>>,
        pending: &mut std::collections::HashMap<ScreenId, std::collections::VecDeque<RestartPlan>>,
        operations: &mut tokio::task::JoinSet<()>,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
    ) {
        let exists = shared
            .lock()
            .map(|state| {
                state
                    .screens
                    .iter()
                    .any(|screen| screen.screen_id == screen_id)
            })
            .unwrap_or(false);
        if !exists || removing.contains_key(&screen_id) {
            let _ = events.send(HostSessionEvent::Wire(HostMessage::Error {
                request_id,
                screen_id: Some(screen_id),
                message: if exists {
                    "Screen removal is already pending"
                } else {
                    "Screen does not exist"
                }
                .into(),
            }));
            return;
        }
        removing.insert(screen_id, request_id);
        pending.remove(&screen_id);
        set_screen_message(&shared, screen_id, "Removing screen…");
        if let Ok(mut state) = shared.lock() {
            if let Some(screen) = state.screens.iter_mut().find(|s| s.screen_id == screen_id) {
                screen.removing = true;
            }
        }
        if let Some(generation) = shared.lock().ok().and_then(|state| {
            state
                .screens
                .iter()
                .find(|s| s.screen_id == screen_id)
                .map(|s| s.generation)
        }) {
            let _ = events.send(HostSessionEvent::Wire(HostMessage::StreamStatus {
                screen_id,
                generation,
                lane: MediaLane::Video,
                state: MediaState::Stopped,
                message: "Screen is being removed".into(),
            }));
        }
        // A close during reconfiguration wins over restarting that screen.
        if restarting.contains(&screen_id) {
            return;
        }
        restarting.insert(screen_id);
        let previous = streams.remove(&screen_id);
        operations.spawn(async move {
            let result = if let Some(mut stream) = previous {
                stop_running_stream(screen_id, &mut stream).await
            } else {
                Ok(())
            };
            let _ = events.send(HostSessionEvent::RemoveFinished {
                screen_id,
                request_id,
                result: result.map_err(|error| format!("{error:#}")),
            });
        });
    }

    #[derive(Clone, Copy)]
    enum RestartKind {
        Added,
        SettingsApplied,
    }

    struct RestartPlan {
        screen_id: ScreenId,
        request_id: Option<RequestId>,
        kind: RestartKind,
        settings: VideoSettings,
        codec: CodecOption,
        revision: Option<ConfigRevision>,
    }

    fn install_screen_settings(
        shared: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        settings: VideoSettings,
        codec: CodecOption,
        requested_revision: Option<ConfigRevision>,
    ) -> Result<ScreenConfig> {
        settings.validate()?;
        let mut settings = settings;
        settings.audio = false;
        let mut state = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("Host state is unavailable"))?;
        let (name, revision, generation) = {
            let screen = state
                .screens
                .iter_mut()
                .find(|screen| screen.screen_id == screen_id)
                .ok_or_else(|| anyhow::anyhow!("Screen {screen_id} does not exist"))?;
            if let Some(revision) = requested_revision {
                if revision <= screen.revision {
                    bail!("Settings revision {revision} is stale for screen {screen_id}");
                }
                screen.revision = revision;
            } else {
                screen.revision = screen.revision.saturating_add(1).max(1);
            }
            screen.generation = screen.generation.saturating_add(1).max(1);
            screen.settings = settings.clone();
            screen.codec = Some(codec);
            screen.status = "Starting updated stream…".into();
            screen.frames = 0;
            screen.dropped = 0;
            screen.bytes = 0;
            (screen.name.clone(), screen.revision, screen.generation)
        };
        if state.navigation.selected == Some(screen_id) {
            state.settings = settings.clone();
        }
        let instrumented = state
            .screens
            .iter()
            .find(|s| s.screen_id == screen_id)
            .is_some_and(|s| s.instrumented());
        if let Some(screen) = state.screens.iter_mut().find(|s| s.screen_id == screen_id) {
            screen.telemetry = Telemetry::new();
            screen.stats_view.reset();
        }
        Ok(ScreenConfig {
            instrumented,
            screen_id,
            name,
            revision,
            generation,
            settings,
            codec,
        })
    }

    fn insert_screen(shared: &Arc<Mutex<State>>, settings: VideoSettings) -> Result<ScreenId> {
        settings.validate()?;
        let mut state = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("Host state is unavailable"))?;
        if state.screens.len() >= MAX_SCREENS {
            bail!("A maximum of {MAX_SCREENS} screens is supported");
        }
        let screen_id = state.next_screen_id;
        state.next_screen_id = state.next_screen_id.saturating_add(1);
        let name = format!("Screen {screen_id}");
        state.screens.push(HostScreen {
            screen_id,
            name,
            revision: 0,
            generation: 0,
            settings: settings.clone(),
            codec: None,
            status: "Prepared · waiting for a viewer".into(),
            removing: false,
            frames: 0,
            dropped: 0,
            bytes: 0,
            diagnostics: Arc::new(Diagnostics::new(&format!("host-screen-{screen_id}"))),
            telemetry: Telemetry::new(),
            stats_view: StatsView::default(),
            viewer_stats: false,
        });
        state.navigation.open(screen_id);
        state.show_codecs = false;
        state.show_logs = false;
        state.settings = settings;
        state.navigation.view = ScreenView::Details;
        Ok(screen_id)
    }

    fn current_screen_settings(
        shared: &Arc<Mutex<State>>,
        screen_id: ScreenId,
    ) -> Result<VideoSettings> {
        shared
            .lock()
            .map_err(|_| anyhow::anyhow!("Host state is unavailable"))?
            .screens
            .iter()
            .find(|screen| screen.screen_id == screen_id)
            .map(|screen| screen.settings.clone())
            .ok_or_else(|| anyhow::anyhow!("Screen {screen_id} does not exist"))
    }

    struct StreamWorkerGuard {
        abort: tokio::task::AbortHandle,
        stop: Arc<AtomicBool>,
    }
    impl Drop for StreamWorkerGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.abort.abort();
        }
    }

    fn spawn_reported_stream(
        screen_id: ScreenId,
        generation: StreamGeneration,
        stop: Arc<AtomicBool>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
        work: impl std::future::Future<Output = Result<()>> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let worker = tokio::spawn(work);
        // Capture the guard before spawning the supervisor, so aborting even
        // an unpolled supervisor also cancels its child and signals capture.
        let guard = StreamWorkerGuard {
            abort: worker.abort_handle(),
            stop,
        };
        tokio::spawn(async move {
            let _guard = guard;
            let error = match worker.await {
                Ok(result) => result.err().map(|error| format!("{error:#}")),
                Err(error) => Some(format!("Video worker stopped unexpectedly: {error}")),
            };
            let _ = events.send(HostSessionEvent::Finished {
                screen_id,
                generation,
                error,
            });
        })
    }

    fn start_screen_stream(
        connection: quinn::Connection,
        screen: ScreenConfig,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
    ) -> RunningStream {
        let screen_id = screen.screen_id;
        let generation = screen.generation;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_capture = stop.clone();
        let (cancel, cancel_rx) = oneshot::channel();
        let task_events = events.clone();
        let task_stop = stop.clone();
        let task = spawn_reported_stream(
            screen_id,
            generation,
            task_stop,
            task_events,
            run_screen_stream(connection, screen, shared, events, cancel_rx, stop_capture),
        );
        RunningStream {
            generation,
            stop,
            cancel: Some(cancel),
            task,
        }
    }

    async fn stop_running_stream(screen_id: ScreenId, stream: &mut RunningStream) -> Result<()> {
        stream.stop.store(true, Ordering::Release);
        if let Some(cancel) = stream.cancel.take() {
            let _ = cancel.send(());
        }
        match tokio::time::timeout(Duration::from_secs(12), &mut stream.task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow::anyhow!(
                "Stream task for screen {screen_id} failed: {error}"
            )),
            Err(_) => {
                stream.task.abort();
                let _ = (&mut stream.task).await;
                Err(anyhow::anyhow!(
                    "Timed out while stopping screen {screen_id}"
                ))
            }
        }
    }

    fn schedule_screen_restart(
        plan: RestartPlan,
        streams: &mut std::collections::HashMap<ScreenId, RunningStream>,
        restarting: &mut std::collections::HashSet<ScreenId>,
        pending: &mut std::collections::HashMap<ScreenId, std::collections::VecDeque<RestartPlan>>,
        operations: &mut tokio::task::JoinSet<()>,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
    ) {
        let screen_id = plan.screen_id;
        if restarting.contains(&screen_id) {
            pending.entry(screen_id).or_default().push_back(plan);
            return;
        }
        if let Some(revision) = plan.revision {
            let current_revision = shared.lock().ok().and_then(|state| {
                state
                    .screens
                    .iter()
                    .find(|screen| screen.screen_id == screen_id)
                    .map(|screen| screen.revision)
            });
            let error = match current_revision {
                Some(current) if revision <= current => Some(format!(
                    "Settings revision {revision} is stale for screen {screen_id}; current revision is {current}"
                )),
                None => Some(format!("Screen {screen_id} does not exist")),
                _ => None,
            };
            if let Some(error) = error {
                let _ = events.send(HostSessionEvent::RestartFinished {
                    screen_id,
                    request_id: plan.request_id,
                    kind: plan.kind,
                    result: Err(error),
                });
                return;
            }
        }
        restarting.insert(screen_id);
        let previous = streams.remove(&screen_id);
        operations.spawn(async move {
            let result: Result<ScreenConfig> = async {
                if let Some(mut stream) = previous {
                    stop_running_stream(screen_id, &mut stream).await?;
                }
                install_screen_settings(
                    &shared,
                    screen_id,
                    plan.settings,
                    plan.codec,
                    plan.revision,
                )
            }
            .await;
            let _ = events.send(HostSessionEvent::RestartFinished {
                screen_id,
                request_id: plan.request_id,
                kind: plan.kind,
                result: result.map_err(|error| format!("{error:#}")),
            });
        });
    }

    async fn run_screen_stream(
        connection: quinn::Connection,
        screen: ScreenConfig,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
        cancel: oneshot::Receiver<()>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        if screen.instrumented {
            run_screen_stream_impl::<true>(connection, screen, shared, events, cancel, stop).await
        } else {
            run_screen_stream_impl::<false>(connection, screen, shared, events, cancel, stop).await
        }
    }
    async fn run_screen_stream_impl<const DEBUG: bool>(
        connection: quinn::Connection,
        screen: ScreenConfig,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<HostSessionEvent>,
        mut cancel: oneshot::Receiver<()>,
        stop: Arc<AtomicBool>,
    ) -> Result<()> {
        let screen_id = screen.screen_id;
        let generation = screen.generation;
        set_screen_generation_message(
            &shared,
            screen_id,
            generation,
            "Opening independent video channel…",
        );
        let mut media_send = connection.open_uni().await?;
        send_json(
            &mut media_send,
            &MediaStreamHello {
                version: VERSION,
                screen_id,
                generation,
                lane: MediaLane::Video,
            },
        )
        .await?;
        let _ = events.send(HostSessionEvent::Wire(HostMessage::StreamStatus {
            screen_id,
            generation,
            lane: MediaLane::Video,
            state: MediaState::Starting,
            message: "Preparing video capture".into(),
        }));

        let (packet_send, mut packet_recv) = mpsc::channel::<mac::EncodedPacket>(4);
        let dropped = Arc::new(AtomicU64::new(0));
        let settings = screen.settings.clone();
        let codec = screen.codec;
        let telemetry = shared
            .lock()
            .unwrap()
            .screens
            .iter()
            .find(|s| s.screen_id == screen_id)
            .unwrap()
            .telemetry
            .clone();
        let capture_telemetry = telemetry.clone();
        let capture_state = shared.clone();
        let capture_stop = stop.clone();
        let capture_dropped = dropped.clone();
        let mut capture = tokio::task::spawn_blocking(move || {
            mac::capture::<DEBUG>(
                screen_id,
                generation,
                settings,
                codec,
                packet_send,
                capture_stop,
                capture_dropped,
                capture_telemetry,
                |progress| {
                    set_screen_generation_message(&capture_state, screen_id, generation, progress)
                },
            )
        });

        let mut first_frame = true;
        let mut capture_result = None;
        let mut stream_error = None;
        let mut stats_timer = tokio::time::interval(Duration::from_secs(1));
        stats_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_stats_at = tokio::time::Instant::now();
        let mut last_stats_bytes = 0u64;
        let mut last_stats_frames = 0;
        let mut last_rtt = 0u64;
        loop {
            tokio::select! {
                _ = &mut cancel => break,
                _ = connection.closed() => break,
                stopped = media_send.stopped() => {
                    stream_error = Some(anyhow::anyhow!("Viewer closed the video lane: {stopped:?}"));
                    break;
                }
                result = &mut capture, if capture_result.is_none() => {
                    capture_result = Some(result);
                    break;
                }
                packet = packet_recv.recv(), if capture_result.is_none() => {
                    let Some(packet) = packet else { break; };
                    if packet.header.screen_id != screen_id || packet.header.generation != generation {
                        stream_error = Some(anyhow::anyhow!("Capture emitted a packet for a different screen generation"));
                        break;
                    }
                    let generation_is_current = shared.lock().map(|state| {
                        state.screens.iter().any(|entry| {
                            entry.screen_id == screen_id && entry.generation == generation
                        })
                    }).unwrap_or(false);
                    if !generation_is_current {
                        break;
                    }
                    let write_start=if DEBUG {now_us()} else {0};
                    if DEBUG {if let Some(t)=&packet.trace {t.mark(Stage::SendQueue,packet.queued_us,Some(write_start),Outcome::Complete,""); t.mark(Stage::Write,write_start,None,Outcome::Pending,"");}}
                    let send = async {
                        if DEBUG {send_debug_media(&mut media_send,&packet.header,packet.trace.as_ref().map(|t|t.capture_us),write_start,&packet.bytes).await}
                        else {send_media(&mut media_send,&packet.header,&packet.bytes).await}
                    };
                    let sent = tokio::select! {
                        _ = &mut cancel => break,
                        _ = connection.closed() => break,
                        result = tokio::time::timeout(Duration::from_secs(5), send) => {
                            result.map_err(|_| anyhow::anyhow!("Viewer stopped accepting video for five seconds")).and_then(|result| result)
                        }
                    };
                    if DEBUG {if let Some(t)=&packet.trace {t.mark(Stage::Write,write_start,Some(now_us()),if sent.is_ok(){Outcome::Complete}else{Outcome::Failed},&sent.as_ref().err().map(|e|e.to_string()).unwrap_or_default());}}
                    if let Err(error) = sent {
                        stream_error = Some(error);
                        break;
                    }
                    if let Ok(mut state) = shared.lock() {
                        if let Some(entry) = state.screens.iter_mut().find(|entry| entry.screen_id == screen_id && entry.generation == generation) {
                            entry.bytes = entry.bytes.saturating_add(packet.bytes.len() as u64);
                            entry.frames = entry.frames.saturating_add(1);
                            entry.dropped = dropped.load(Ordering::Relaxed);

                        }
                    }
                    if first_frame {
                        first_frame = false;
                        set_screen_generation_message(&shared, screen_id, generation, "Streaming video");
                        let _ = events.send(HostSessionEvent::Wire(HostMessage::StreamStatus {
                            screen_id,
                            generation,
                            lane: MediaLane::Video,
                            state: MediaState::Ready,
                            message: "Video frames are flowing".into(),
                        }));
                    }
                }
                _ = stats_timer.tick() => {
                    let (frames, bytes) = shared.lock().ok()
                        .and_then(|state| state.screens.iter().find(|entry| entry.screen_id == screen_id)
                            .map(|entry| (entry.frames, entry.bytes)))
                        .unwrap_or_default();
                    let elapsed = last_stats_at.elapsed().as_secs_f64().max(0.001);
                    let bitrate_kbps = (((bytes.saturating_sub(last_stats_bytes) as f64 * 8.0) / elapsed) / 1000.0)
                        .round().clamp(0.0, u32::MAX as f64) as u32;
                    if DEBUG {
                        let rtt=connection.rtt().as_micros() as u64;
                        let transport=connection.stats();
                        telemetry.stats(StreamStats {at_us:now_us(),bitrate_kbps:bitrate_kbps as f64,fps:frames.saturating_sub(last_stats_frames) as f64/elapsed,frames,dropped:dropped.load(Ordering::Relaxed),rtt_us:rtt,jitter_us:rtt.abs_diff(last_rtt),lost_packets:transport.path.lost_packets,congestion_events:transport.path.congestion_events,send_queue:Some((packet_recv.max_capacity()-packet_recv.capacity()) as u32),..Default::default()});
                        telemetry.expire(now_us() as i64,"Stage did not conclude within five seconds");
                        last_rtt=rtt; last_stats_frames=frames;
                    }
                    last_stats_at = tokio::time::Instant::now();
                    last_stats_bytes = bytes;
                    let _ = events.send(HostSessionEvent::Wire(HostMessage::Stats {
                        screen_id,
                        generation,
                        frames_sent: frames,
                        frames_dropped: dropped.load(Ordering::Relaxed),
                        bitrate_kbps,
                    }));
                }
            }
        }

        stop.store(true, Ordering::Release);
        drop(packet_recv);
        if capture_result.is_none() {
            capture_result = Some(capture.await);
        }
        if DEBUG {
            let reason = stream_error
                .as_ref()
                .map(|e| e.to_string())
                .or_else(|| {
                    capture_result.as_ref().and_then(|r| match r {
                        Ok(Err(e)) => Some(e.to_string()),
                        Err(e) => Some(e.to_string()),
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "Stream stopped or restarted".into());
            telemetry.finish(now_us() as i64, &reason);
        }
        let _ = media_send.finish();
        if let Some(error) = stream_error {
            return Err(error);
        }
        match capture_result.expect("capture task result is set") {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.context(format!("Capture failed for screen {screen_id}"))),
            Err(error) => Err(anyhow::anyhow!(
                "Capture task for screen {screen_id} failed: {error}"
            )),
        }
    }

    async fn handle_connection(
        connection: quinn::Connection,
        shared: Arc<Mutex<State>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        set_message(&shared, "Authenticating viewer…");
        let pin = shared.lock().unwrap().pin.clone();
        let setup = async {
            let authentication = tokio::time::timeout(
                Duration::from_secs(12),
                authenticate_host(&connection, &pin),
            )
            .await
            .map_err(|_| anyhow::anyhow!("PIN entry timed out"))?;
            if authentication.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            authentication?;
            let (control_send, mut control_recv) =
                tokio::time::timeout(Duration::from_secs(8), connection.accept_bi())
                    .await
                    .map_err(|_| anyhow::anyhow!("Viewer did not open a control channel"))??;
            let hello: ClientMessage =
                tokio::time::timeout(Duration::from_secs(8), receive_json(&mut control_recv))
                    .await
                    .map_err(|_| anyhow::anyhow!("Viewer did not start a session"))??;
            Ok::<_, anyhow::Error>((control_send, control_recv, hello))
        };
        let (mut control_send, control_recv, hello) = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            result = setup => result?,
        };
        let ClientHello {
            version,
            codecs: viewer_options,
            initial_settings: requested_settings,
        } = match hello {
            ClientMessage::Hello(hello) => hello,
            _ => bail!("Expected a viewer greeting"),
        };
        if version != VERSION {
            bail!("Viewer uses protocol version {version}; this host requires {VERSION}");
        }
        if viewer_options.is_empty() {
            bail!("Viewer did not advertise any usable video codecs");
        }
        let (ui_commands, mut ui_rx) = mpsc::unbounded_channel::<HostUiCommand>();
        let primary_id = match shared
            .lock()
            .ok()
            .and_then(|state| state.screens.first().map(|screen| screen.screen_id))
        {
            Some(id) => id,
            None => insert_screen(&shared, built_in_presets()[1].settings.clone())?,
        };
        let mut settings = match requested_settings {
            Some(settings) => settings,
            None => current_screen_settings(&shared, primary_id)?,
        };
        let audio_requested = settings.audio;
        settings.audio = false;
        settings.validate()?;
        let host_options = mac::available_codecs();
        let option = negotiate(&host_options, &viewer_options, settings.preferred_codec)?;
        let primary = install_screen_settings(&shared, primary_id, settings.clone(), option, None)?;
        send_control(
            &mut control_send,
            &HostMessage::Accepted {
                screen: primary.clone(),
            },
        )
        .await?;
        if let Ok(mut state) = shared.lock() {
            state.settings = settings.clone();
            state.session_commands = Some(ui_commands);
            state.viewer_name = Some(connection.remote_address().to_string());
            state.selected_stats_viewer = state.viewer_name.clone();
            state.frames = 0;
            state.dropped = 0;
            state.bytes = 0;
            state.audio_notice =
                audio_requested.then(|| "Audio unavailable · continuing with video only".into());
            if let Some(notice) = &state.audio_notice {
                state.diagnostics.record("host", notice);
            }
        }

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<HostSessionEvent>();
        let mut streams = std::collections::HashMap::<ScreenId, RunningStream>::new();
        let mut restarting = std::collections::HashSet::<ScreenId>::new();
        let mut removing = std::collections::HashMap::<ScreenId, Option<RequestId>>::new();
        let mut pending_restarts =
            std::collections::HashMap::<ScreenId, std::collections::VecDeque<RestartPlan>>::new();
        streams.insert(
            primary_id,
            start_screen_stream(
                connection.clone(),
                primary,
                shared.clone(),
                event_tx.clone(),
            ),
        );

        let stats_state = shared.clone();
        let diagnostic_send = tokio::spawn(bridge_core::telemetry_transport::send_batches(
            connection.clone(),
            move || {
                stats_state
                    .lock()
                    .map(|s| {
                        s.screens
                            .iter()
                            .filter(|s| s.stats_view.open || s.viewer_stats)
                            .map(|s| (s.screen_id, s.generation, s.telemetry.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        ));
        let diagnostic_connection = connection.clone();
        let diagnostic_state = shared.clone();
        let diagnostic_receive = tokio::spawn(async move {
            while let Ok(mut stream) = diagnostic_connection.accept_uni().await {
                let hello = tokio::time::timeout(
                    Duration::from_secs(5),
                    receive_json::<_, MediaStreamHello>(&mut stream),
                )
                .await;
                if !matches!(
                    hello,
                    Ok(Ok(MediaStreamHello {
                        version: VERSION,
                        lane: MediaLane::Diagnostics,
                        ..
                    }))
                ) {
                    let _ = stream.stop(0u32.into());
                    continue;
                }
                let state = diagnostic_state.clone();
                let _ = bridge_core::telemetry_transport::receive_batches(stream, move |batch| {
                    ingest_diagnostic_batch(&state, batch)
                })
                .await;
            }
        });
        let (mut control_messages, control_task) = control_reader::<_, ClientMessage>(control_recv);
        let mut operations = tokio::task::JoinSet::new();
        // Every fallible session operation is inside this block so cleanup always runs.
        let session_result: Result<()> = async {
        // Screens prepared before the viewer connected are announced and
        // started independently on their own QUIC video streams.
        let prepared_ids = shared
            .lock()
            .map(|state| {
                state
                    .screens
                    .iter()
                    .map(|screen| screen.screen_id)
                    .filter(|id| *id != primary_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for screen_id in prepared_ids {
            let prepared = current_screen_settings(&shared, screen_id).and_then(|settings| {
                let codec = negotiate(&host_options, &viewer_options, settings.preferred_codec)?;
                install_screen_settings(&shared, screen_id, settings, codec, None)
            });
            match prepared {
                Ok(screen) => {
                    send_control(
                        &mut control_send,
                        &HostMessage::ScreenAdded {
                            request_id: None,
                            screen: screen.clone(),
                        },
                    )
                    .await?;
                    streams.insert(
                        screen_id,
                        start_screen_stream(
                            connection.clone(),
                            screen,
                            shared.clone(),
                            event_tx.clone(),
                        ),
                    );
                }
                Err(error) => {
                    let detail = format!("Prepared stream unavailable: {error:#}");
                    set_screen_message(&shared, screen_id, &detail);
                    send_control(
                        &mut control_send,
                        &HostMessage::Error {
                            request_id: None,
                            screen_id: Some(screen_id),
                            message: detail,
                        },
                    )
                    .await?;
                }
            }
        }

        set_message(&shared, format!("Streaming {} screen(s)", streams.len()));
        loop {
            tokio::select! {
                _ = shutdown.changed() => break Ok(()),
                reason = connection.closed() => break Err(anyhow::anyhow!("Viewer disconnected: {reason}")),
                completed = operations.join_next(), if !operations.is_empty() => {
                    if let Some(Err(error)) = completed { break Err(anyhow::anyhow!("Screen operation task failed: {error}")); }
                }
                event = event_rx.recv() => {
                    match event {
                        Some(HostSessionEvent::Wire(message)) => send_control(&mut control_send, &message).await?,
                        Some(HostSessionEvent::RestartFinished { screen_id, request_id, kind, result }) => {
                            restarting.remove(&screen_id);
                            if let Some(request_id) = removing.remove(&screen_id) {
                                pending_restarts.remove(&screen_id);
                                remove_host_screen(&shared, screen_id);
                                send_control(&mut control_send, &HostMessage::ScreenRemoved { request_id, screen_id }).await?;
                                continue;
                            }
                            match result {
                                Ok(screen) => {
                                    let response = match kind {
                                        RestartKind::Added => HostMessage::ScreenAdded { request_id, screen: screen.clone() },
                                        RestartKind::SettingsApplied => HostMessage::ScreenSettingsApplied { request_id, screen: screen.clone() },
                                    };
                                    send_control(&mut control_send, &response).await?;
                                    streams.insert(screen_id, start_screen_stream(connection.clone(), screen, shared.clone(), event_tx.clone()));
                                }
                                Err(error) => {
                                    set_screen_message(&shared, screen_id, &format!("Stream update failed: {error}"));
                                    if matches!(kind, RestartKind::Added) {
                                        if let Ok(mut state) = shared.lock() {
                                            state.screens.retain(|screen| screen.screen_id != screen_id);
                                            let fallback = state.screens.first().map(|screen| screen.screen_id);
                                            state.navigation.remove(screen_id, fallback);
                                        }
                                    }
                                    send_control(&mut control_send, &HostMessage::Error {
                                        request_id,
                                        screen_id: Some(screen_id),
                                        message: format!("Stream update failed: {error}"),
                                    }).await?;
                                }
                            }
                            let next = pending_restarts.get_mut(&screen_id).and_then(|queue| queue.pop_front());
                            if pending_restarts.get(&screen_id).is_some_and(|queue| queue.is_empty()) {
                                pending_restarts.remove(&screen_id);
                            }
                            if let Some(plan) = next {
                                schedule_screen_restart(plan, &mut streams, &mut restarting, &mut pending_restarts, &mut operations, shared.clone(), event_tx.clone());
                            }
                        }
                        Some(HostSessionEvent::RemoveFinished { screen_id, request_id, result }) => {
                            restarting.remove(&screen_id);
                            removing.remove(&screen_id);
                            match result {
                                Ok(()) => {
                                    remove_host_screen(&shared, screen_id);
                                    send_control(&mut control_send, &HostMessage::ScreenRemoved { request_id, screen_id }).await?;
                                }
                                Err(error) => {
                                    if let Ok(mut state) = shared.lock() { if let Some(screen) = state.screens.iter_mut().find(|s| s.screen_id == screen_id) { screen.removing = false; screen.status = format!("Could not remove screen: {error}"); } }
                                    send_control(&mut control_send, &HostMessage::Error { request_id, screen_id: Some(screen_id), message: format!("Could not remove screen: {error}") }).await?;
                                }
                            }
                        }
                        Some(HostSessionEvent::Finished { screen_id, generation, error }) => {
                            let current = streams.get(&screen_id).is_some_and(|stream| stream.generation == generation);
                            if current {
                                if let Some(mut stream) = streams.remove(&screen_id) {
                                    let _ = (&mut stream.task).await;
                                }
                                let (state, message) = match error {
                                    Some(error) => (MediaState::Failed, format!("Video stream failed: {error}")),
                                    None => (MediaState::Stopped, "Video stream stopped".into()),
                                };
                                set_screen_message(&shared, screen_id, &message);
                                send_control(&mut control_send, &HostMessage::StreamStatus {
                                    screen_id, generation, lane: MediaLane::Video, state, message,
                                }).await?;
                            }
                        }
                        None => {},
                    }
                }
                ui_command = ui_rx.recv() => {
                    match ui_command {
                        Some(HostUiCommand::AddScreen(screen_id)) => {
                            if !streams.contains_key(&screen_id) && !restarting.contains(&screen_id) {
                                match current_screen_settings(&shared, screen_id)
                                    .and_then(|settings| {
                                        let option = negotiate(&host_options, &viewer_options, settings.preferred_codec)?;
                                        Ok((settings, option))
                                    }) {
                                    Ok((settings, option)) => {
                                        schedule_screen_restart(
                                            RestartPlan { screen_id, request_id: None, kind: RestartKind::Added, settings, codec: option, revision: None },
                                            &mut streams, &mut restarting, &mut pending_restarts, &mut operations,
                                            shared.clone(), event_tx.clone(),
                                        );
                                    }
                                    Err(error) => send_control(&mut control_send, &HostMessage::Error { request_id: None, screen_id: Some(screen_id), message: format!("Cannot prepare screen: {error:#}") }).await?,
                                }
                            }
                        }
                        Some(HostUiCommand::ChangeSettings(screen_id, settings)) => {
                            if removing.contains_key(&screen_id) { continue; }
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => schedule_screen_restart(
                                    RestartPlan { screen_id, request_id: None, kind: RestartKind::SettingsApplied, settings, codec, revision: None },
                                    &mut streams, &mut restarting, &mut pending_restarts, &mut operations,
                                    shared.clone(), event_tx.clone(),
                                ),
                                Err(error) => send_control(&mut control_send, &HostMessage::Error { request_id: None, screen_id: Some(screen_id), message: format!("Settings change failed: {error:#}") }).await?,
                            }
                        }
                        Some(HostUiCommand::RemoveScreen(screen_id)) => {
                            schedule_screen_removal(screen_id, None, &mut streams, &mut restarting,
                                &mut removing, &mut pending_restarts, &mut operations, shared.clone(), event_tx.clone());
                        }
                        Some(HostUiCommand::Diagnostics(screen_id,enabled)) => {
                            if removing.contains_key(&screen_id) {continue;}
                            let restart=shared.lock().ok().and_then(|mut state|{let s=state.screens.iter_mut().find(|s|s.screen_id==screen_id)?;s.stats_view.open=enabled;Some((s.settings.clone(),s.codec?))});
                            if let Some((settings,codec))=restart {schedule_screen_restart(RestartPlan{screen_id,request_id:None,kind:RestartKind::SettingsApplied,settings,codec,revision:None},&mut streams,&mut restarting,&mut pending_restarts,&mut operations,shared.clone(),event_tx.clone());}
                        }
                        Some(HostUiCommand::Burst(id))=>{if let Ok(state)=shared.lock(){if let Some(s)=state.screens.iter().find(|s|s.screen_id==id){s.telemetry.burst();}}}
                        Some(HostUiCommand::Disconnect) => break Ok(()),
                        None => break Ok(()),
                    }
                }
                command = control_messages.recv() => {
                    match command {
                        Some(Ok(ClientMessage::ClockProbe{t1}))=>{let t2=now_us();send_control(&mut control_send,&HostMessage::ClockReply{t1,t2,t3:now_us()}).await?;}
                        Some(Ok(ClientMessage::TraceBurst{screen_id}))=>{if let Ok(state)=shared.lock(){if let Some(s)=state.screens.iter().find(|s|s.screen_id==screen_id){s.telemetry.burst();}}}
                        Some(Ok(ClientMessage::Diagnostics{screen_id,enabled}))=>{
                            if removing.contains_key(&screen_id) {continue;}
                            let restart=shared.lock().ok().and_then(|mut state|{let s=state.screens.iter_mut().find(|s|s.screen_id==screen_id)?;s.viewer_stats=enabled;Some((s.settings.clone(),s.codec?))});
                            if let Some((settings,codec))=restart{schedule_screen_restart(RestartPlan{screen_id,request_id:None,kind:RestartKind::SettingsApplied,settings,codec,revision:None},&mut streams,&mut restarting,&mut pending_restarts,&mut operations,shared.clone(),event_tx.clone());}
                        }
                        Some(Ok(ClientMessage::Stop)) => break Ok(()),
                        Some(Ok(ClientMessage::AddScreen(AddScreenRequest { request_id, settings }))) => {
                            let mut settings = settings.unwrap_or_else(|| shared.lock().map(|state| state.settings.clone()).unwrap_or_else(|_| built_in_presets()[1].settings.clone()));
                            settings.audio = false;
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => match insert_screen(&shared, settings.clone()) {
                                    Ok(screen_id) => schedule_screen_restart(
                                        RestartPlan { screen_id, request_id: Some(request_id), kind: RestartKind::Added, settings, codec, revision: None },
                                        &mut streams, &mut restarting, &mut pending_restarts, &mut operations,
                                        shared.clone(), event_tx.clone(),
                                    ),
                                    Err(error) => send_control(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: None, message: format!("Cannot add screen: {error:#}") }).await?,
                                },
                                Err(error) => send_control(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: None, message: format!("Cannot add screen: {error:#}") }).await?,
                            }
                        }
                        Some(Ok(ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest { request_id, screen_id, revision, mut settings }))) => {
                            settings.audio = false;
                            if removing.contains_key(&screen_id) {
                                send_control(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: "The screen is being removed".into() }).await?;
                                continue;
                            }
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => schedule_screen_restart(
                                    RestartPlan { screen_id, request_id: Some(request_id), kind: RestartKind::SettingsApplied, settings, codec, revision: Some(revision) },
                                    &mut streams, &mut restarting, &mut pending_restarts, &mut operations,
                                    shared.clone(), event_tx.clone(),
                                ),
                                Err(error) => send_control(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: format!("Settings change failed: {error:#}") }).await?,
                            }
                        }
                        Some(Ok(ClientMessage::RemoveScreen { request_id, screen_id })) => {
                            schedule_screen_removal(screen_id, Some(request_id), &mut streams, &mut restarting,
                                &mut removing, &mut pending_restarts, &mut operations, shared.clone(), event_tx.clone());
                        }
                        Some(Ok(ClientMessage::Hello(_))) => send_control(&mut control_send, &HostMessage::Error { request_id: None, screen_id: None, message: "Hello may only be sent once".into() }).await?,
                        Some(Err(error)) => break Err(anyhow::anyhow!("Viewer control channel closed: {error}")),
                        None => break Err(anyhow::anyhow!("Viewer control channel ended")),
                    }
                }
            }
        }
        }.await;
        // Notify the peer immediately, even if an OS capture callback takes time to stop.
        connection.close(0u32.into(), b"host session ended");
        // Cancelling owned operations drops their stream guards and stops capture.
        operations.abort_all();
        while operations.join_next().await.is_some() {}
        diagnostic_send.abort();
        diagnostic_receive.abort();
        let _ = diagnostic_send.await;
        let _ = diagnostic_receive.await;
        control_task.abort();
        let _ = control_task.await;
        let active_streams = std::mem::take(&mut streams);
        let mut cleanup = tokio::task::JoinSet::new();
        for (screen_id, mut stream) in active_streams {
            cleanup.spawn(async move {
                let result = stop_running_stream(screen_id, &mut stream).await;
                (screen_id, result)
            });
        }
        while let Some(result) = cleanup.join_next().await {
            match result {
                Ok((screen_id, Err(error))) => {
                    set_screen_message(&shared, screen_id, &format!("Stream cleanup: {error:#}"));
                }
                Err(error) => set_message(&shared, format!("Stream cleanup task failed: {error}")),
                _ => {}
            }
        }
        if let Ok(mut state) = shared.lock() {
            state.session_commands = None;
            state.viewer_name = None;
            state.selected_stats_viewer = None;
            state.navigation.back();
            state.show_codecs = false;
            state.show_logs = false;
            state.frames = 0;
            state.dropped = 0;
            state.bytes = 0;
            for screen in &mut state.screens {
                screen.status = "Prepared · waiting for a viewer".into();
                screen.removing = false;
                screen.frames = 0;
                screen.dropped = 0;
                screen.bytes = 0;
            }
        }
        session_result?;
        Ok(())
    }

    fn initial_state(presets: Vec<Preset>, pin: String) -> State {
        State {
            pin,
            viewer_name: None,
            selected_stats_viewer: None,
            message: "Starting…".into(),
            settings: presets[1].settings.clone(),
            screens: vec![HostScreen {
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
    #[cfg(test)]
    mod tests {
        use super::*;
        fn state() -> Arc<Mutex<State>> {
            Arc::new(Mutex::new(initial_state(
                built_in_presets(),
                "123456".into(),
            )))
        }
        #[test]
        fn stats_owners_are_independent_and_restarts_change_generation() {
            let shared = state();
            let settings = current_screen_settings(&shared, 1).unwrap();
            let codec = mac::available_codecs()[0];
            let normal =
                install_screen_settings(&shared, 1, settings.clone(), codec, None).unwrap();
            assert!(!normal.instrumented);
            shared.lock().unwrap().screens[0].stats_view.open = true;
            let debug = install_screen_settings(&shared, 1, settings.clone(), codec, None).unwrap();
            assert!(debug.instrumented);
            assert!(debug.generation > normal.generation);
            {
                let mut state = shared.lock().unwrap();
                state.screens[0].viewer_stats = true;
                state.screens[0].stats_view.open = false;
            }
            assert!(
                install_screen_settings(&shared, 1, settings.clone(), codec, None)
                    .unwrap()
                    .instrumented
            );
            shared.lock().unwrap().screens[0].viewer_stats = false;
            assert!(
                !install_screen_settings(&shared, 1, settings, codec, None)
                    .unwrap()
                    .instrumented
            );
        }

        #[test]
        fn removing_selected_and_last_host_screen_repairs_navigation() {
            let shared = state();
            let second = insert_screen(&shared, built_in_presets()[0].settings.clone()).unwrap();
            {
                let mut state = shared.lock().unwrap();
                state.show_codecs = true;
                state.show_logs = true;
            }
            remove_host_screen(&shared, second);
            {
                let state = shared.lock().unwrap();
                assert_eq!(state.navigation.selected, Some(1));
                assert_eq!(state.navigation.view, ScreenView::List);
                assert!(!state.show_logs && !state.show_codecs);
            }
            remove_host_screen(&shared, 1);
            remove_host_screen(&shared, 1);
            let state = shared.lock().unwrap();
            assert!(state.screens.is_empty());
            assert_eq!(state.navigation.selected, None);
        }
        #[tokio::test]
        async fn dropping_stream_guard_stops_capture_and_aborts_task() {
            let stop = Arc::new(AtomicBool::new(false));
            let (cancel, cancelled) = oneshot::channel();
            let task = tokio::spawn(std::future::pending::<()>());
            let aborted = task.abort_handle();
            drop(RunningStream {
                generation: 1,
                stop: stop.clone(),
                cancel: Some(cancel),
                task,
            });
            assert!(stop.load(Ordering::Acquire));
            assert!(cancelled.await.is_ok());
            tokio::task::yield_now().await;
            assert!(aborted.is_finished());
        }
        #[tokio::test]
        async fn aborting_the_supervisor_also_drops_its_media_worker() {
            struct Dropped(Option<oneshot::Sender<()>>);
            impl Drop for Dropped {
                fn drop(&mut self) {
                    if let Some(send) = self.0.take() {
                        let _ = send.send(());
                    }
                }
            }
            let stop = Arc::new(AtomicBool::new(false));
            let (send, _receive) = mpsc::unbounded_channel();
            let (dropped, did_drop) = oneshot::channel();
            let notify = Dropped(Some(dropped));
            let task = spawn_reported_stream(4, 9, stop.clone(), send, async move {
                let _notify = notify;
                std::future::pending::<()>().await;
                Ok(())
            });
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            tokio::time::timeout(Duration::from_secs(1), did_drop)
                .await
                .unwrap()
                .unwrap();
            assert!(stop.load(Ordering::Acquire));
        }

        #[tokio::test]
        async fn worker_panic_reports_its_screen_and_generation_and_stops_capture() {
            let stop = Arc::new(AtomicBool::new(false));
            let (send, mut receive) = mpsc::unbounded_channel();
            let task = spawn_reported_stream(4, 9, stop.clone(), send, async {
                panic!("simulated video worker failure")
            });
            task.await.unwrap();
            match receive.recv().await.unwrap() {
                HostSessionEvent::Finished {
                    screen_id,
                    generation,
                    error,
                } => {
                    assert_eq!((screen_id, generation), (4, 9));
                    assert!(error.unwrap().contains("panicked"));
                }
                _ => panic!("unexpected event"),
            }
            assert!(stop.load(Ordering::Acquire));
        }

        #[tokio::test]
        async fn removal_during_restart_discards_queued_restarts() {
            let shared = state();
            let mut streams = std::collections::HashMap::new();
            let mut restarting = std::collections::HashSet::from([1]);
            let mut removing = std::collections::HashMap::new();
            let mut pending = std::collections::HashMap::new();
            let codec = CodecOption {
                codec: bridge_core::config::Codec::H264,
                acceleration: bridge_core::config::Acceleration::Software,
            };
            pending.insert(
                1,
                std::collections::VecDeque::from([RestartPlan {
                    screen_id: 1,
                    request_id: Some(11),
                    kind: RestartKind::SettingsApplied,
                    settings: built_in_presets()[1].settings.clone(),
                    codec,
                    revision: Some(3),
                }]),
            );
            let mut operations = tokio::task::JoinSet::new();
            let (send, mut receive) = mpsc::unbounded_channel();
            schedule_screen_removal(
                1,
                Some(12),
                &mut streams,
                &mut restarting,
                &mut removing,
                &mut pending,
                &mut operations,
                shared.clone(),
                send.clone(),
            );
            assert_eq!(removing.get(&1), Some(&Some(12)));
            assert!(pending.is_empty());
            assert!(operations.is_empty());
            assert!(matches!(
                receive.recv().await,
                Some(HostSessionEvent::Wire(HostMessage::StreamStatus {
                    state: MediaState::Stopped,
                    ..
                }))
            ));
            schedule_screen_removal(
                1,
                Some(13),
                &mut streams,
                &mut restarting,
                &mut removing,
                &mut pending,
                &mut operations,
                shared,
                send,
            );
            assert!(matches!(
                receive.recv().await,
                Some(HostSessionEvent::Wire(HostMessage::Error {
                    request_id: Some(13),
                    ..
                }))
            ));
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    app::main();
}
