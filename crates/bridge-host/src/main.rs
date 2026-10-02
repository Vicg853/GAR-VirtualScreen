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
        protocol::{
            AddScreenRequest, ChangeScreenSettingsRequest, ClientHello, ClientMessage,
            ConfigRevision, HostMessage, MAX_SCREENS, MediaLane, MediaState, MediaStreamHello,
            PORT, RequestId, ScreenConfig, ScreenId, StreamGeneration, VERSION, receive_json,
            send_json, send_media,
        },
        security::{authenticate_host, server_endpoint},
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
    use tokio::sync::{mpsc, oneshot};

    #[derive(Clone)]
    struct HostScreen {
        screen_id: ScreenId,
        name: String,
        revision: ConfigRevision,
        generation: StreamGeneration,
        settings: VideoSettings,
        codec: Option<CodecOption>,
        status: String,
        frames: u64,
        dropped: u64,
        bytes: u64,
        diagnostics: Arc<Diagnostics>,
    }

    enum HostUiCommand {
        AddScreen(ScreenId),
        ChangeSettings(ScreenId, VideoSettings),
    }

    #[derive(Clone)]
    struct State {
        pin: String,
        message: String,
        settings: VideoSettings,
        screens: Vec<HostScreen>,
        selected_screen: ScreenId,
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
        show_screen_details: bool,
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
        let screen_id = state.selected_screen;
        let settings = state.settings.clone();
        let screen = state
            .screens
            .iter_mut()
            .find(|screen| screen.screen_id == screen_id)?;
        screen.settings = settings.clone();
        screen.revision = screen.revision.saturating_add(1);
        screen.status = "Applying updated settings…".into();
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
            frames: 0,
            dropped: 0,
            bytes: 0,
            diagnostics,
        });
        state.selected_screen = screen_id;
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
                    action(&mut state);
                    let update = sync_selected_settings(&mut state);
                    if label != "Save custom preset" {
                        state.message = format!("{label} updated · applying to this screen");
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
            let state = self.state.lock().unwrap().clone();
            let selected_screen = state
                .screens
                .iter()
                .find(|screen| screen.screen_id == state.selected_screen)
                .cloned();
            let screen_cards = state
                .screens
                .iter()
                .map(|screen| {
                    let shared = self.state.clone();
                    let screen_id = screen.screen_id;
                    let selected = screen_id == state.selected_screen;
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
                                state.selected_screen = screen_id;
                                if let Some((settings, name)) =
                                    state.screens.iter().find(|s| s.screen_id == screen_id).map(
                                        |screen| (screen.settings.clone(), screen.name.clone()),
                                    )
                                {
                                    state.settings = settings;
                                    state.message = format!("Selected {name}");
                                    state.show_screen_details = true;
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
                                state.show_screen_details = true;
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
                .child(if state.show_screen_details {
                    "← Screen list"
                } else {
                    "Selected screen details →"
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Ok(mut state) = this.state.lock() {
                        state.show_screen_details = true;
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
                        state.show_screen_details = false;
                    }
                    cx.notify();
                }));
            let screen_list_panel = if state.show_screen_details {
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
                    .child(details_toggle)
            };
            let screen_details = if state.show_screen_details {
                div()
                    .id("host-screen-details")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(list_toggle)
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
                                .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                        ),
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
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(0xa8b8cf))
                                .child("PAIRING PIN"),
                        )
                        .child(div().text_xl().child(state.pin.clone()))
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

    fn run_server(shared: Arc<Mutex<State>>) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let endpoint =
                server_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), PORT))?;
            let _advertiser = Advertiser::new("AirBridge Mac")?;
            set_message(&shared, "Ready for a Linux viewer");
            while let Some(incoming) = endpoint.accept().await {
                let connection = match incoming.await {
                    Ok(connection) => connection,
                    Err(error) => {
                        set_message(&shared, format!("Connection failed: {error}"));
                        continue;
                    }
                };
                let result = handle_connection(connection.clone(), shared.clone()).await;
                match result {
                    Ok(()) => set_message(&shared, "Viewer disconnected. Ready to reconnect."),
                    Err(error) => set_message(&shared, format!("Connection error: {error:#}")),
                }
                connection.close(0u32.into(), b"session ended");
            }
            Ok(())
        })
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
            request_id: RequestId,
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
            } else if screen.revision == 0 {
                screen.revision = 1;
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
        if state.selected_screen == screen_id {
            state.settings = settings.clone();
        }
        Ok(ScreenConfig {
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
            frames: 0,
            dropped: 0,
            bytes: 0,
            diagnostics: Arc::new(Diagnostics::new(&format!("host-screen-{screen_id}"))),
        });
        state.selected_screen = screen_id;
        state.settings = settings;
        state.show_screen_details = true;
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
        let task = tokio::spawn(async move {
            let result =
                run_screen_stream(connection, screen, shared, events, cancel_rx, stop_capture)
                    .await;
            let error = result.err().map(|error| format!("{error:#}"));
            let _ = task_events.send(HostSessionEvent::Finished {
                screen_id,
                generation,
                error,
            });
            task_stop.store(true, Ordering::Release);
        });
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
        tokio::spawn(async move {
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
        let capture_state = shared.clone();
        let capture_stop = stop.clone();
        let capture_dropped = dropped.clone();
        let mut capture = tokio::task::spawn_blocking(move || {
            mac::capture(
                screen_id,
                generation,
                settings,
                codec,
                packet_send,
                capture_stop,
                capture_dropped,
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
        loop {
            tokio::select! {
                _ = &mut cancel => break,
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
                    if let Err(error) = send_media(&mut media_send, &packet.header, &packet.bytes).await {
                        stream_error = Some(error);
                        break;
                    }
                    if let Ok(mut state) = shared.lock() {
                        if let Some(entry) = state.screens.iter_mut().find(|entry| entry.screen_id == screen_id && entry.generation == generation) {
                            entry.bytes = entry.bytes.saturating_add(packet.bytes.len() as u64);
                            entry.frames = entry.frames.saturating_add(1);
                            entry.dropped = dropped.load(Ordering::Relaxed);
                            if entry.frames == 1 || entry.frames % 120 == 0 {
                                entry.diagnostics.record(
                                    "stream",
                                    format!("Sent {} frames · {:.1} MB", entry.frames, entry.bytes as f64 / 1_000_000.0),
                                );
                            }
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
    ) -> Result<()> {
        set_message(&shared, "Authenticating viewer…");
        let pin = shared.lock().unwrap().pin.clone();
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
        let (mut control_send, mut control_recv) =
            tokio::time::timeout(Duration::from_secs(8), connection.accept_bi())
                .await
                .map_err(|_| anyhow::anyhow!("Viewer did not open a control channel"))??;
        let hello: ClientMessage =
            tokio::time::timeout(Duration::from_secs(8), receive_json(&mut control_recv))
                .await
                .map_err(|_| anyhow::anyhow!("Viewer did not start a session"))??;
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
        let mut settings = match requested_settings {
            Some(settings) => settings,
            None => current_screen_settings(&shared, 1)?,
        };
        let audio_requested = settings.audio;
        settings.audio = false;
        settings.validate()?;
        let host_options = mac::available_codecs();
        let option = negotiate(&host_options, &viewer_options, settings.preferred_codec)?;
        let primary = install_screen_settings(&shared, 1, settings.clone(), option, None)?;
        send_json(
            &mut control_send,
            &HostMessage::Accepted {
                screen: primary.clone(),
            },
        )
        .await?;
        if let Ok(mut state) = shared.lock() {
            state.settings = settings.clone();
            state.session_commands = Some(ui_commands);
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
        let mut removing = std::collections::HashSet::<ScreenId>::new();
        let mut pending_restarts =
            std::collections::HashMap::<ScreenId, std::collections::VecDeque<RestartPlan>>::new();
        streams.insert(
            1,
            start_screen_stream(
                connection.clone(),
                primary,
                shared.clone(),
                event_tx.clone(),
            ),
        );

        // Screens prepared before the viewer connected are announced and
        // started independently on their own QUIC video streams.
        let prepared_ids = shared
            .lock()
            .map(|state| {
                state
                    .screens
                    .iter()
                    .map(|screen| screen.screen_id)
                    .filter(|id| *id != 1)
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
                    send_json(
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
                    send_json(
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
        let session_result: Result<()> = loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Some(HostSessionEvent::Wire(message)) => send_json(&mut control_send, &message).await?,
                        Some(HostSessionEvent::RestartFinished { screen_id, request_id, kind, result }) => {
                            restarting.remove(&screen_id);
                            match result {
                                Ok(screen) => {
                                    let response = match kind {
                                        RestartKind::Added => HostMessage::ScreenAdded { request_id, screen: screen.clone() },
                                        RestartKind::SettingsApplied => HostMessage::ScreenSettingsApplied { request_id, screen: screen.clone() },
                                    };
                                    send_json(&mut control_send, &response).await?;
                                    streams.insert(screen_id, start_screen_stream(connection.clone(), screen, shared.clone(), event_tx.clone()));
                                }
                                Err(error) => {
                                    set_screen_message(&shared, screen_id, &format!("Stream update failed: {error}"));
                                    if matches!(kind, RestartKind::Added) {
                                        if let Ok(mut state) = shared.lock() {
                                            state.screens.retain(|screen| screen.screen_id != screen_id);
                                        }
                                    }
                                    send_json(&mut control_send, &HostMessage::Error {
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
                                schedule_screen_restart(plan, &mut streams, &mut restarting, &mut pending_restarts, shared.clone(), event_tx.clone());
                            }
                        }
                        Some(HostSessionEvent::RemoveFinished { screen_id, request_id, result }) => {
                            restarting.remove(&screen_id);
                            removing.remove(&screen_id);
                            match result {
                                Ok(()) => {
                                    if let Ok(mut state) = shared.lock() {
                                        state.screens.retain(|screen| screen.screen_id != screen_id);
                                        if state.selected_screen == screen_id {
                                            state.selected_screen = 1;
                                            if let Some(primary) = state.screens.iter().find(|screen| screen.screen_id == 1) {
                                                state.settings = primary.settings.clone();
                                            }
                                        }
                                    }
                                    send_json(&mut control_send, &HostMessage::ScreenRemoved { request_id: Some(request_id), screen_id }).await?;
                                }
                                Err(error) => {
                                    send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: format!("Could not remove screen: {error}") }).await?;
                                }
                            }
                        }
                        Some(HostSessionEvent::Finished { screen_id, generation, error }) => {
                            let current = streams.get(&screen_id).is_some_and(|stream| stream.generation == generation);
                            if current {
                                if let Some(stream) = streams.remove(&screen_id) {
                                    let _ = stream.task.await;
                                }
                                let (state, message) = match error {
                                    Some(error) => (MediaState::Failed, format!("Video stream failed: {error}")),
                                    None => (MediaState::Stopped, "Video stream stopped".into()),
                                };
                                set_screen_message(&shared, screen_id, &message);
                                send_json(&mut control_send, &HostMessage::StreamStatus {
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
                                            &mut streams, &mut restarting, &mut pending_restarts,
                                            shared.clone(), event_tx.clone(),
                                        );
                                    }
                                    Err(error) => send_json(&mut control_send, &HostMessage::Error { request_id: None, screen_id: Some(screen_id), message: format!("Cannot prepare screen: {error:#}") }).await?,
                                }
                            }
                        }
                        Some(HostUiCommand::ChangeSettings(screen_id, settings)) => {
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => schedule_screen_restart(
                                    RestartPlan { screen_id, request_id: None, kind: RestartKind::SettingsApplied, settings, codec, revision: None },
                                    &mut streams, &mut restarting, &mut pending_restarts,
                                    shared.clone(), event_tx.clone(),
                                ),
                                Err(error) => send_json(&mut control_send, &HostMessage::Error { request_id: None, screen_id: Some(screen_id), message: format!("Settings change failed: {error:#}") }).await?,
                            }
                        }
                        None => {},
                    }
                }
                command = receive_json::<_, ClientMessage>(&mut control_recv) => {
                    match command {
                        Ok(ClientMessage::Stop) => break Ok(()),
                        Ok(ClientMessage::AddScreen(AddScreenRequest { request_id, settings })) => {
                            let mut settings = settings.unwrap_or_else(|| shared.lock().map(|state| state.settings.clone()).unwrap_or_else(|_| built_in_presets()[1].settings.clone()));
                            settings.audio = false;
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => match insert_screen(&shared, settings.clone()) {
                                    Ok(screen_id) => schedule_screen_restart(
                                        RestartPlan { screen_id, request_id: Some(request_id), kind: RestartKind::Added, settings, codec, revision: None },
                                        &mut streams, &mut restarting, &mut pending_restarts,
                                        shared.clone(), event_tx.clone(),
                                    ),
                                    Err(error) => send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: None, message: format!("Cannot add screen: {error:#}") }).await?,
                                },
                                Err(error) => send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: None, message: format!("Cannot add screen: {error:#}") }).await?,
                            }
                        }
                        Ok(ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest { request_id, screen_id, revision, mut settings })) => {
                            settings.audio = false;
                            if removing.contains(&screen_id) {
                                send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: "The screen is being removed".into() }).await?;
                                continue;
                            }
                            let result = settings.validate().and_then(|()| {
                                negotiate(&host_options, &viewer_options, settings.preferred_codec)
                            });
                            match result {
                                Ok(codec) => schedule_screen_restart(
                                    RestartPlan { screen_id, request_id: Some(request_id), kind: RestartKind::SettingsApplied, settings, codec, revision: Some(revision) },
                                    &mut streams, &mut restarting, &mut pending_restarts,
                                    shared.clone(), event_tx.clone(),
                                ),
                                Err(error) => send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: format!("Settings change failed: {error:#}") }).await?,
                            }
                        }
                        Ok(ClientMessage::RemoveScreen { request_id, screen_id }) => {
                            if screen_id == 1 {
                                send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: "The primary screen cannot be removed".into() }).await?;
                            } else if restarting.contains(&screen_id) || removing.contains(&screen_id) {
                                send_json(&mut control_send, &HostMessage::Error { request_id: Some(request_id), screen_id: Some(screen_id), message: "The screen is already being updated".into() }).await?;
                            } else {
                                restarting.insert(screen_id);
                                removing.insert(screen_id);
                                let previous = streams.remove(&screen_id);
                                let events = event_tx.clone();
                                tokio::spawn(async move {
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
                        }
                        Ok(ClientMessage::Hello(_)) => send_json(&mut control_send, &HostMessage::Error { request_id: None, screen_id: None, message: "Hello may only be sent once".into() }).await?,
                        Err(error) => break Err(anyhow::anyhow!("Viewer control channel closed: {error}")),
                    }
                }
            }
        };
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
            for screen in &mut state.screens {
                screen.status = "Prepared · waiting for a viewer".into();
            }
        }
        session_result?;
        Ok(())
    }

    pub fn main() {
        let mut presets = built_in_presets();
        if let Ok(custom) = load_custom_presets() {
            presets.extend(custom);
        }
        let pin = random_pin().expect("System randomness is unavailable");
        let state = Arc::new(Mutex::new(State {
            pin,
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
                frames: 0,
                dropped: 0,
                bytes: 0,
                diagnostics: Arc::new(Diagnostics::new("host-screen-1")),
            }],
            selected_screen: 1,
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
            show_screen_details: false,
            diagnostics: Arc::new(Diagnostics::new("host")),
        }));
        state
            .lock()
            .unwrap()
            .diagnostics
            .record("host", mac::capture_permission_diagnostic());
        let server_state = state.clone();
        std::thread::spawn(move || {
            if let Err(error) = run_server(server_state.clone()) {
                set_message(&server_state, format!("Server could not start: {error:#}"));
            }
        });
        Application::new().run(move |cx: &mut App| {
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
    }
}

#[cfg(target_os = "macos")]
fn main() {
    app::main();
}
