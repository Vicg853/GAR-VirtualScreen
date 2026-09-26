mod linux;

mod app {
    use super::linux::{AudioPlayer, VideoDecoder, VideoFrame};
    use anyhow::{Result, bail};
    use bridge_core::{
        config::{
            Acceleration, Codec, CodecOption, Preset, VideoSettings, built_in_presets,
            load_custom_presets, save_custom_presets,
        },
        discovery::{DiscoveredHost, browse},
        protocol::{Control, MediaKind, VERSION, receive_json, receive_media, send_json},
        security::{authenticate_viewer, client_endpoint},
    };
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyDownEvent, ObjectFit, Window,
        WindowBounds, WindowOptions, div, img, prelude::*, px, rgb, size,
    };
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::mpsc;

    enum Command {
        Connect(SocketAddr, String, Option<VideoSettings>),
        Disconnect,
    }

    #[derive(Clone)]
    struct State {
        hosts: Vec<DiscoveredHost>,
        selected_host: usize,
        pin: String,
        manual_address: String,
        edit_address: bool,
        settings: VideoSettings,
        override_host: bool,
        presets: Vec<Preset>,
        selected_preset: usize,
        message: String,
        show_codecs: bool,
        bytes: u64,
    }

    fn message(state: &Arc<Mutex<State>>, text: impl Into<String>) {
        if let Ok(mut state) = state.lock() {
            state.message = text.into();
        }
    }

    struct ViewerView {
        state: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        commands: mpsc::UnboundedSender<Command>,
        focus: FocusHandle,
    }

    fn setting_button(
        label: &'static str,
        id: &'static str,
        shared: Arc<Mutex<State>>,
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
                    action(&mut state);
                    state.override_host = true;
                }
                cx.notify();
            }))
    }

    impl Render for ViewerView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let state = self.state.lock().unwrap().clone();
            let frame = self.frame.lock().unwrap();
            let displayed = frame.image.clone();
            let decoded = frame.frames;
            let dropped = frame.dropped;
            let frame_error = frame.error.clone();
            drop(frame);
            let hosts = state
                .hosts
                .iter()
                .enumerate()
                .map(|(index, host)| {
                    let shared = self.state.clone();
                    let selected = index == state.selected_host;
                    div()
                        .id(("host", index))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(if selected { 0x365986 } else { 0x28354b }))
                        .cursor_pointer()
                        .child(host.name.clone())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                state.selected_host = index;
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
                        .cursor_pointer()
                        .child(preset.name.clone())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Ok(mut state) = shared.lock() {
                                state.selected_preset = index;
                                state.settings = state.presets[index].settings.clone();
                                state.override_host = true;
                            }
                            cx.notify();
                        }))
                })
                .collect::<Vec<_>>();
            let connect_state = self.state.clone();
            let connect_commands = self.commands.clone();
            let codec_panel = if state.show_codecs {
                let shared = self.state.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0x28354b))
                    .child("Decoder: FFmpeg hardware auto, software fallback")
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
                                "Toggle audio",
                                "audio",
                                shared.clone(),
                                |s| s.settings.audio = !s.settings.audio,
                                cx,
                            ))
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
            let preview = if let Some(image) = displayed {
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(rgb(0x05080d))
                    .child(img(image).object_fit(ObjectFit::Contain).size_full())
            } else {
                div()
                    .flex_1()
                    .bg(rgb(0x05080d))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("The Mac display will appear here")
            };
            div().size_full().track_focus(&self.focus).on_key_down(cx.listener(Self::on_key)).bg(rgb(0x141c2b)).text_color(rgb(0xf2f5fb)).flex().flex_col()
                .child(div().p_3().flex().flex_col().gap_2()
                    .child(div().text_xl().child("AirBridge · Linux viewer"))
                    .child(div().flex().gap_2().children(hosts))
                    .child(format!("PIN: {}  ·  type six digits", state.pin))
                    .child(format!("Manual address: {}{}", state.manual_address, if state.edit_address { "  [editing]" } else { "" }))
                    .child(div().id("edit-address").px_3().py_2().rounded_md().bg(rgb(0x28354b)).cursor_pointer().child("Edit address / PIN")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Ok(mut state) = this.state.lock() { state.edit_address = !state.edit_address; }
                            cx.notify();
                        })))
                    .child(div().flex().gap_2().children(presets))
                    .child(div().id("codec-menu").px_3().py_2().rounded_md().bg(rgb(0x28354b)).cursor_pointer().child("Codec settings ▾")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Ok(mut state) = this.state.lock() { state.show_codecs = !state.show_codecs; }
                            cx.notify();
                        })))
                    .child(codec_panel)
                    .child(div().flex().gap_2()
                        .child(div().id("connect").px_4().py_2().rounded_md().bg(rgb(0x3777b6)).cursor_pointer().child("Connect")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                let state = connect_state.lock().unwrap().clone();
                                let address = if !state.manual_address.is_empty() {
                                    state.manual_address.parse::<SocketAddr>().ok().or_else(|| state.manual_address.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, bridge_core::protocol::PORT)))
                                } else { state.hosts.get(state.selected_host).and_then(|host| host.addresses.first().copied()) };
                                if let Some(address) = address { let _ = connect_commands.send(Command::Connect(address, state.pin, if state.override_host { Some(state.settings) } else { None })); }
                                else { message(&connect_state, "Select a discovered Mac or enter its IP address"); }
                                cx.notify();
                            })))
                        .child(div().id("disconnect").px_4().py_2().rounded_md().bg(rgb(0x573846)).cursor_pointer().child("Disconnect")
                            .on_click(cx.listener(|this, _, _, _| { let _ = this.commands.send(Command::Disconnect); })))))
                .child(preview)
                .child(div().p_3().child(format!("{} · decoded: {} · dropped: {} · received: {:.1} MB{}", state.message, decoded, dropped, state.bytes as f64 / 1_000_000.0, frame_error.map(|e| format!(" · {e}")).unwrap_or_default())))
        }
    }

    impl ViewerView {
        fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
            if let Ok(mut state) = self.state.lock() {
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

    fn run_network(
        shared: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        updates: mpsc::UnboundedSender<()>,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            while let Some(command) = commands.recv().await {
                if let Command::Connect(address, pin, settings) = command {
                    if let Err(error) = connect(
                        address,
                        pin,
                        settings,
                        shared.clone(),
                        frame.clone(),
                        updates.clone(),
                        &mut commands,
                    )
                    .await
                    {
                        message(&shared, format!("Connection failed: {error:#}"));
                    }
                }
            }
            Ok(())
        })
    }

    async fn connect(
        address: SocketAddr,
        pin: String,
        settings: Option<VideoSettings>,
        shared: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        updates: mpsc::UnboundedSender<()>,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) -> Result<()> {
        if let Some(settings) = &settings {
            settings.validate()?;
        }
        bridge_core::security::validate_pin(&pin)?;
        message(&shared, format!("Connecting to {address}…"));
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
        let options = vec![
            CodecOption {
                codec: Codec::H264,
                acceleration: Acceleration::Hardware,
            },
            CodecOption {
                codec: Codec::H264,
                acceleration: Acceleration::Software,
            },
        ];
        send_json(
            &mut control_send,
            &Control::Hello {
                version: VERSION,
                options,
                settings,
            },
        )
        .await?;
        let accepted: Control = receive_json(&mut control_recv).await?;
        let (option, actual) = match accepted {
            Control::Accepted { option, settings } => (option, settings),
            Control::Error(text) => bail!("Host: {text}"),
            _ => bail!("Host sent an invalid response"),
        };
        let mut media_recv = connection.accept_uni().await?;
        *frame.lock().unwrap() = VideoFrame::default();
        let decoder = VideoDecoder::new(
            actual.width,
            actual.height,
            frame.clone(),
            updates,
            option.acceleration == Acceleration::Hardware,
        )?;
        let player = if actual.audio {
            match AudioPlayer::new() {
                Ok(player) => Some(player),
                Err(error) => {
                    message(
                        &shared,
                        format!("Video connected; audio unavailable: {error}"),
                    );
                    None
                }
            }
        } else {
            None
        };
        message(
            &shared,
            format!(
                "Streaming {}×{} at {} fps",
                actual.width, actual.height, actual.fps
            ),
        );
        loop {
            tokio::select! {
                command = commands.recv() => { if matches!(command, Some(Command::Disconnect) | None) { break; } }
                packet = receive_media(&mut media_recv) => {
                    let (header, bytes) = packet?;
                    if let Ok(mut state) = shared.lock() { state.bytes += bytes.len() as u64; }
                    match header.kind {
                        MediaKind::Video => {
                            if decoder.submit(bytes).is_err() {
                                if let Ok(mut frame) = frame.lock() { frame.dropped += 1; }
                            }
                        }
                        MediaKind::Audio => { if let Some(player) = &player { player.submit(&bytes, header.sample_rate, header.channels); } }
                    }
                }
            }
        }
        send_json(&mut control_send, &Control::Stop).await.ok();
        connection.close(0u32.into(), b"viewer disconnected");
        message(&shared, "Disconnected");
        Ok(())
    }

    pub fn main() {
        let mut presets = built_in_presets();
        if let Ok(custom) = load_custom_presets() {
            presets.extend(custom);
        }
        let state = Arc::new(Mutex::new(State {
            hosts: Vec::new(),
            selected_host: 0,
            pin: String::new(),
            manual_address: String::new(),
            edit_address: false,
            settings: presets[1].settings.clone(),
            override_host: false,
            presets,
            selected_preset: 1,
            message: "Discovering Macs…".into(),
            show_codecs: false,
            bytes: 0,
        }));
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        let (send, receive) = mpsc::unbounded_channel();
        let (frame_updates, mut frame_receiver) = mpsc::unbounded_channel();
        let network_state = state.clone();
        let network_frame = frame.clone();
        std::thread::spawn(move || {
            if let Err(error) =
                run_network(network_state.clone(), network_frame, frame_updates, receive)
            {
                message(&network_state, format!("Network error: {error:#}"));
            }
        });
        let browse_state = state.clone();
        std::thread::spawn(move || match browse() {
            Ok(found) => {
                while let Ok(host) = found.recv() {
                    if let Ok(mut state) = browse_state.lock() {
                        if let Some(existing) = state
                            .hosts
                            .iter_mut()
                            .find(|existing| existing.name == host.name)
                        {
                            *existing = host;
                        } else {
                            state.hosts.push(host);
                        }
                    }
                }
            }
            Err(error) => message(&browse_state, format!("Discovery unavailable: {error}")),
        });
        Application::new().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(1100.0), px(800.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let focus = cx.focus_handle();
                    window.focus(&focus);
                    let view = cx.new(|_| ViewerView {
                        state: state.clone(),
                        frame: frame.clone(),
                        commands: send.clone(),
                        focus,
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
                    let weak = view.downgrade();
                    cx.spawn(async move |cx| {
                        while frame_receiver.recv().await.is_some() {
                            if weak.update(cx, |_, cx| cx.notify()).is_err() {
                                break;
                            }
                        }
                    })
                    .detach();
                    view
                },
            )
            .expect("Cannot open AirBridge viewer");
            cx.activate(true);
        });
    }
}

fn main() {
    app::main();
}
