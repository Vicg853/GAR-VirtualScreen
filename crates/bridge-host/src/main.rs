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
        discovery::Advertiser,
        protocol::{Control, MediaKind, PORT, VERSION, receive_json, send_json, send_media},
        security::{authenticate_host, server_endpoint},
    };
    use gpui::{
        App, Application, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*,
        px, rgb, size,
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
    }

    fn set_message(state: &Arc<Mutex<State>>, message: impl Into<String>) {
        if let Ok(mut state) = state.lock() {
            state.message = message.into();
        }
    }

    struct HostView {
        state: Arc<Mutex<State>>,
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
                        .bg(rgb(0x263248))
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                state.selected = index;
                                state.settings = state.presets[index].settings.clone();
                                state.message =
                                    "Preset selected. It applies to the next connection.".into();
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
            div()
                .size_full()
                .p_5()
                .bg(rgb(0x141c2b))
                .text_color(rgb(0xf2f5fb))
                .flex()
                .flex_col()
                .gap_3()
                .child(div().text_xl().child("AirBridge · Mac host"))
                .child(format!("Connect from Linux with PIN {}", state.pin))
                .child(format!(
                    "{}×{} · {} fps · video only",
                    state.settings.width, state.settings.height, state.settings.fps,
                ))
                .child(div().flex().gap_2().children(presets))
                .child(
                    div()
                        .id("codec-menu")
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x35517a))
                        .cursor_pointer()
                        .child("Codec settings ▾")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Ok(mut state) = this.state.lock() {
                                state.show_codecs = !state.show_codecs;
                            }
                            cx.notify();
                        })),
                )
                .child(codec_panel)
                .child(format!("Status: {}", state.message))
                .child(format!(
                    "Frames: {} · dropped: {} · sent: {:.1} MB",
                    state.frames,
                    state.dropped,
                    state.bytes as f64 / 1_000_000.0
                ))
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
        loop {
            tokio::select! {
                packet = rx.recv() => {
                    let Some(packet) = packet else { break; };
                    send_media(&mut media_send, &packet.header, &packet.bytes).await?;
                    if let Ok(mut state) = shared.lock() {
                        state.bytes += packet.bytes.len() as u64;
                        if matches!(packet.header.kind, MediaKind::Video) { state.frames += 1; }
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
        stop.store(true, Ordering::Relaxed);
        capture_task.await??;
        let _ = media_send.finish();
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
        }));
        let server_state = state.clone();
        std::thread::spawn(move || {
            if let Err(error) = run_server(server_state.clone()) {
                set_message(&server_state, format!("Server could not start: {error:#}"));
            }
        });
        Application::new().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(740.0), px(600.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
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
