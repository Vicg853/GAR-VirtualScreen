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
            Preset, VideoSettings, built_in_presets, load_custom_presets, negotiate,
            save_custom_presets,
        },
        diagnostics::Diagnostics,
        discovery::Advertiser,
        protocol::{Control, MediaKind, PORT, VERSION, receive_json, send_json, send_media},
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
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct State {
        pin: String,
        message: String,
        settings: VideoSettings,
        presets: Vec<Preset>,
        selected: usize,
        frames: u64,
        dropped: u64,
        bytes: u64,
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
                    if label != "Save custom preset" {
                        state.message = format!("{label} updated · applies on the next connection");
                    }
                    state.diagnostics.record("settings", &state.message);
                }
                cx.notify();
            }))
    }

    impl Render for HostView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let state = self.state.lock().unwrap().clone();
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
                                state.message =
                                    "Preset selected. It applies to the next connection.".into();
                                state.diagnostics.record(
                                    "settings",
                                    format!("Selected {} preset", state.presets[index].name),
                                );
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
                        state
                            .diagnostics
                            .path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "unavailable".into())
                    ))
                    .children(
                        state
                            .diagnostics
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
                                .bg(rgb(0x203149))
                                .child(format!("Status: {}", state.message)),
                        )
                        .child(format!(
                            "Sent {} frames · {} capture drops · {:.1} MB",
                            state.frames,
                            state.dropped,
                            state.bytes as f64 / 1_000_000.0
                        )),
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
        let hello: Control =
            tokio::time::timeout(Duration::from_secs(8), receive_json(&mut control_recv))
                .await
                .map_err(|_| anyhow::anyhow!("Viewer did not start a session"))??;
        let (viewer_options, requested_settings) = match hello {
            Control::Hello {
                version: VERSION,
                options,
                settings,
            } => (options, settings),
            Control::Hello { .. } => bail!("Viewer uses an incompatible protocol version"),
            _ => bail!("Expected a viewer greeting"),
        };
        let mut settings =
            requested_settings.unwrap_or_else(|| shared.lock().unwrap().settings.clone());
        settings.audio = false;
        settings.validate()?;
        let option = negotiate(
            &mac::available_codecs(),
            &viewer_options,
            settings.preferred_codec,
        )?;
        send_json(
            &mut control_send,
            &Control::Accepted {
                option,
                settings: settings.clone(),
            },
        )
        .await?;
        if let Ok(mut state) = shared.lock() {
            state.settings = settings.clone();
            state.frames = 0;
            state.dropped = 0;
            state.bytes = 0;
        }
        let mut media_send = connection.open_uni().await?;
        let stop = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(4);
        set_message(
            &shared,
            format!(
                "Streaming {}×{} at {} fps",
                settings.width, settings.height, settings.fps
            ),
        );
        let settings_capture = settings.clone();
        let stop_capture = stop.clone();
        let dropped_capture = dropped.clone();
        let capture_task = tokio::task::spawn_blocking(move || {
            mac::capture(settings_capture, option, tx, stop_capture, dropped_capture)
        });
        let session_result: Result<()> = async {
            loop {
                tokio::select! {
                packet = rx.recv() => {
                    let Some(packet) = packet else { break; };
                    send_media(&mut media_send, &packet.header, &packet.bytes).await?;
                    if let Ok(mut state) = shared.lock() {
                        state.bytes += packet.bytes.len() as u64;
                        if matches!(packet.header.kind, MediaKind::Video) {
                            state.frames += 1;
                            if state.frames == 1 || state.frames % 120 == 0 {
                                state.diagnostics.record("host", format!("Sent {} video frames, {:.1} MB", state.frames, state.bytes as f64 / 1_000_000.0));
                            }
                        }
                        state.dropped = dropped.load(Ordering::Relaxed);
                    }
                }
                command = receive_json::<_, Control>(&mut control_recv) => {
                    match command {
                        Ok(Control::Stop) => break,
                        Ok(Control::ChangeSettings(_)) => {
                            send_json(&mut control_send, &Control::Error("Reconnect to apply new display settings".into())).await?;
                        }
                        Ok(_) => {},
                        Err(_) => break,
                    }
                }
                }
            }
            Ok(())
        }.await;
        stop.store(true, Ordering::Relaxed);
        let capture_result = capture_task.await?;
        let _ = media_send.finish();
        session_result?;
        capture_result?;
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
            presets,
            selected: 1,
            frames: 0,
            dropped: 0,
            bytes: 0,
            show_codecs: false,
            show_logs: false,
            diagnostics: Arc::new(Diagnostics::new("host")),
        }));
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
