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
        discovery::{DiscoveredHost, browse},
        protocol::{
            AddScreenRequest, ChangeScreenSettingsRequest, ClientHello, ClientMessage, HostMessage,
            MAX_SCREENS, MediaKind, MediaLane, MediaStreamHello, RequestId, ScreenConfig, ScreenId,
            StreamGeneration, VERSION, receive_json, receive_media, send_json,
        },
        security::{authenticate_viewer, client_endpoint},
    };
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, ResizeEdge,
        Window, WindowBounds, WindowDecorations, WindowHandle, WindowOptions, bounds, div, point,
        prelude::*, px, rgb, size,
    };
    use std::{
        collections::HashSet,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::sync::mpsc;

    enum Command {
        Connect(SocketAddr, String, Option<VideoSettings>),
        Disconnect,
        AddScreen,
        ChangeScreenSettings(ScreenId, VideoSettings),
    }

    enum LaneNotice {
        Opened(ScreenId, MediaLane),
        FirstFrame(ScreenId),
        Failed(ScreenId, MediaLane, String),
        Closed(ScreenId, MediaLane),
    }

    #[derive(Clone)]
    struct ClientScreen {
        config: ScreenConfig,
        status: String,
        frame: Arc<Mutex<VideoFrame>>,
        video_window: VideoWindow,
        diagnostics: Arc<Diagnostics>,
        bytes: u64,
        received_frames: u64,
        host_frames_sent: u64,
        host_frames_dropped: u64,
        bitrate_kbps: u32,
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
        screens: Vec<ClientScreen>,
        selected_screen: Option<ScreenId>,
        next_request_id: u64,
        connected: bool,
        presets: Vec<Preset>,
        selected_preset: usize,
        message: String,
        show_codecs: bool,
        show_logs: bool,
        show_screen_details: bool,
        diagnostics: Arc<Diagnostics>,
        bytes: u64,
        received_frames: u64,
    }

    fn message(state: &Arc<Mutex<State>>, text: impl Into<String>) {
        let text = text.into();
        if let Ok(mut state) = state.lock() {
            state.diagnostics.record("viewer", &text);
            state.message = text;
        }
    }

    fn persist_selected_settings(state: &mut State) -> Option<(ScreenId, VideoSettings)> {
        let screen_id = state.selected_screen?;
        let screen = state
            .screens
            .iter_mut()
            .find(|screen| screen.config.screen_id == screen_id)?;
        screen.config.revision = screen.config.revision.saturating_add(1);
        screen.config.settings = state.settings.clone();
        screen.status = "Applying settings · rebuilding media pipeline…".into();
        Some((screen_id, state.settings.clone()))
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
                    action(&mut state);
                    state.override_host = true;
                    let apply = if label == "Save custom preset" {
                        None
                    } else {
                        persist_selected_settings(&mut state)
                    };
                    if label != "Save custom preset" {
                        state.message = if state.connected && state.selected_screen.is_some() {
                            format!("{label} updated · applying to this screen")
                        } else {
                            format!("{label} updated · applies on the next connection")
                        };
                    }
                    state.diagnostics.record("settings", &state.message);
                    if let Some((screen_id, settings)) = apply {
                        let _ = commands.send(Command::ChangeScreenSettings(screen_id, settings));
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
            let state = self.state.lock().unwrap().clone();
            let commands = self.commands.clone();
            let active_screen = state
                .screens
                .iter()
                .find(|screen| Some(screen.config.screen_id) == state.selected_screen)
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
                                state.selected_host = index;
                                state.message = format!("Selected {}", state.hosts[index].name);
                                state.diagnostics.record("viewer", &state.message);
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
                    let selected = Some(screen_id) == state.selected_screen;
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
                                state.selected_screen = Some(screen_id);
                                state.show_screen_details = true;
                                if let Some((settings, name)) = state
                                    .screens
                                    .iter()
                                    .find(|s| s.config.screen_id == screen_id)
                                    .map(|screen| {
                                        (screen.config.settings.clone(), screen.config.name.clone())
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
                                state.selected_preset = index;
                                state.settings = state.presets[index].settings.clone();
                                state.override_host = true;
                                let apply = persist_selected_settings(&mut state);
                                state.message =
                                    if state.connected && state.selected_screen.is_some() {
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
                                if let Some((screen_id, settings)) = apply {
                                    let _ = commands
                                        .send(Command::ChangeScreenSettings(screen_id, settings));
                                }
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
                        state.show_screen_details = true;
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
                        state.show_screen_details = false;
                    }
                    cx.notify();
                }));
            let screen_list_panel = if state.show_screen_details {
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
                            .child(if state.connected {
                                "+ Add stream"
                            } else {
                                "Connect to add a stream"
                            })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                if add_state
                                    .lock()
                                    .map(|state| state.connected)
                                    .unwrap_or(false)
                                {
                                    let _ = add_commands.send(Command::AddScreen);
                                }
                                cx.notify();
                            })),
                    )
                    .child(list_toggle)
            };
            let stream_details_panel = if state.show_screen_details {
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
                    .child(logs_toggle)
                    .child(logs)
                    .child(
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
                                    .and_then(|state| state.screens.iter().find(|screen| Some(screen.config.screen_id) == state.selected_screen).map(|screen| screen.video_window.clone()))
                                    .map(|window| window.show());
                                if let Some(Err(error)) = result {
                                    message(&this.state, format!("Cannot show display: {error:#}"));
                                }
                                cx.notify();
                            })),
                    )
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
                    .child(format!("Received {} · host sent {} · host drops {} · decoded {} · skipped {} · {} · {:.1} MB · {} kbps{}", active_screen.as_ref().map(|screen| screen.received_frames).unwrap_or(state.received_frames), active_screen.as_ref().map(|screen| screen.host_frames_sent).unwrap_or_default(), active_screen.as_ref().map(|screen| screen.host_frames_dropped).unwrap_or_default(), decoded, skipped, if decoder.is_empty() { "decoder initializing" } else { decoder.as_str() }, active_screen.as_ref().map(|screen| screen.bytes).unwrap_or(state.bytes) as f64 / 1_000_000.0, active_screen.as_ref().map(|screen| screen.bitrate_kbps).unwrap_or_default(),
                        frame_error.map(|e| format!(" · {e}")).unwrap_or_default()))
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
                        .on_click(cx.listener(|_, _, window, _| window.remove_window()))))
                .child(div().px_3().py_2().bg(rgb(0x203149)).text_sm().child(state.message.clone()))
                .child(div().id("viewer-settings-scroll").flex_1().min_h_0().overflow_y_scroll().p_3().flex().flex_col().gap_3()
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
                                let address = if !state.manual_address.is_empty() {
                                    state.manual_address.parse::<SocketAddr>().ok().or_else(|| state.manual_address.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, bridge_core::protocol::PORT)))
                                } else { state.hosts.get(state.selected_host).and_then(|host| host.addresses.first().copied()) };
                                if let Some(address) = address { let _ = connect_commands.send(Command::Connect(address, state.pin, if state.override_host { Some(state.settings) } else { None })); }
                                else { message(&connect_state, "Select a discovered Mac or enter its IP address"); }
                                cx.notify();
                            })))
                        .child(div().id("disconnect").px_4().py_2().rounded_md().bg(rgb(0x573846)).cursor_pointer().child("Disconnect")
                            .on_click(cx.listener(|this, _, _, _| { let _ = this.commands.send(Command::Disconnect); }))))
                    .child(screen_list_panel)
                    .child(stream_details_panel))
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
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            while let Some(command) = commands.recv().await {
                if let Command::Connect(address, pin, settings) = command {
                    if let Err(error) =
                        connect(address, pin, settings, shared.clone(), &mut commands).await
                    {
                        message(&shared, format!("Connection failed: {error:#}"));
                    }
                }
            }
            Ok(())
        })
    }

    async fn new_client_screen(config: ScreenConfig, shared: &Arc<Mutex<State>>) -> ClientScreen {
        let screen_id = config.screen_id;
        let diagnostics = Arc::new(Diagnostics::new(&format!("viewer-screen-{screen_id}")));
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        let window_frame = frame.clone();
        let window_diagnostics = diagnostics.clone();
        let spawned = tokio::task::spawn_blocking(move || {
            VideoWindow::spawn(window_frame, window_diagnostics)
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
            config,
            status: "Waiting for video".into(),
            frame,
            video_window,
            diagnostics,
            bytes: 0,
            received_frames: 0,
            host_frames_sent: 0,
            host_frames_dropped: 0,
            bitrate_kbps: 0,
        }
    }

    async fn install_client_screen(
        shared: &Arc<Mutex<State>>,
        config: ScreenConfig,
        select: bool,
    ) -> Result<()> {
        config.settings.validate()?;
        let existing = shared.lock().ok().and_then(|state| {
            state
                .screens
                .iter()
                .find(|screen| screen.config.screen_id == config.screen_id)
                .cloned()
        });
        let mut screen = match existing {
            Some(screen) => screen,
            None => new_client_screen(config.clone(), shared).await,
        };
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
            if select || state.selected_screen.is_none() {
                state.selected_screen = Some(config.screen_id);
                state.show_screen_details = true;
                state.settings = config.settings.clone();
            }
        }
        Ok(())
    }

    async fn remove_client_screen(shared: &Arc<Mutex<State>>, screen_id: ScreenId) {
        let removed = if let Ok(mut state) = shared.lock() {
            let index = state
                .screens
                .iter()
                .position(|screen| screen.config.screen_id == screen_id);
            let removed = index.map(|index| state.screens.remove(index));
            if state.selected_screen == Some(screen_id) {
                state.selected_screen = state.screens.first().map(|screen| screen.config.screen_id);
                if let Some(screen) = state.screens.first() {
                    state.settings = screen.config.settings.clone();
                }
            }
            removed
        } else {
            None
        };
        if let Some(screen) = removed {
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
        mut media_recv: quinn::RecvStream,
        screen: ClientScreen,
        generation: StreamGeneration,
        shared: Arc<Mutex<State>>,
        events: mpsc::UnboundedSender<LaneNotice>,
    ) -> Result<()> {
        let screen_id = screen.config.screen_id;
        let mut first_frame = true;
        loop {
            let (header, bytes) = receive_media(&mut media_recv).await?;
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
                    if entry.config.generation != generation {
                        return Ok(());
                    }
                    entry.bytes = entry.bytes.saturating_add(bytes.len() as u64);
                    entry.received_frames = entry.received_frames.saturating_add(1);
                    if entry.received_frames == 1 || entry.received_frames % 120 == 0 {
                        entry.diagnostics.record(
                            "video",
                            format!(
                                "Received {} frames · {:.1} MB",
                                entry.received_frames,
                                entry.bytes as f64 / 1_000_000.0
                            ),
                        );
                    }
                    state.bytes = state.bytes.saturating_add(bytes.len() as u64);
                    state.received_frames = state.received_frames.saturating_add(1);
                }
            }
            screen.video_window.submit(bytes, header.keyframe).await?;
            if first_frame {
                first_frame = false;
                let _ = events.send(LaneNotice::FirstFrame(screen_id));
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
                            Ok(Err(error)) => return (0, MediaLane::Video, Err(error.context("Invalid media stream preamble"))),
                            Err(_) => return (0, MediaLane::Video, Err(anyhow::anyhow!("Media stream preamble timed out"))),
                        };
                        let (screen_id, generation, lane) = (hello.screen_id, hello.generation, hello.lane);
                        if hello.version != VERSION {
                            return (screen_id, lane, Err(anyhow::anyhow!("Incompatible media protocol version {}", hello.version)));
                        }
                        if lane != MediaLane::Video {
                            let _ = stream.stop(0u32.into());
                            return (screen_id, lane, Err(anyhow::anyhow!("The {lane:?} lane is not implemented yet")));
                        }
                        let key = (screen_id, generation, lane);
                        if let Ok(mut active) = active.lock() {
                            if !active.insert(key) {
                                let _ = stream.stop(0u32.into());
                                return (screen_id, lane, Err(anyhow::anyhow!("Duplicate video lane for screen {screen_id}, generation {generation}")));
                            }
                        }
                        let outcome = match wait_for_client_screen(&shared, screen_id, generation).await {
                            Ok(screen) => {
                                let _ = events.send(LaneNotice::Opened(screen_id, lane));
                                receive_video_lane(stream, screen, generation, shared, events.clone()).await
                            }
                            Err(error) => Err(error),
                        };
                        if let Ok(mut active) = active.lock() {
                            active.remove(&key);
                        }
                        (screen_id, lane, outcome)
                    });
                }
                finished = tasks.join_next(), if !tasks.is_empty() => {
                    match finished {
                        Some(Ok((screen_id, lane, outcome))) => match outcome {
                            Ok(()) => { let _ = events.send(LaneNotice::Closed(screen_id, lane)); },
                            Err(error) => { let _ = events.send(LaneNotice::Failed(screen_id, lane, format!("{error:#}"))); },
                        },
                        Some(Err(error)) => {
                            let _ = events.send(LaneNotice::Failed(0, MediaLane::Video, format!("Media lane task failed: {error}")));
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
        send_json(
            &mut control_send,
            &ClientMessage::Hello(ClientHello {
                version: VERSION,
                codecs,
                initial_settings: settings,
            }),
        )
        .await?;
        let accepted: HostMessage = receive_json(&mut control_recv).await?;
        let primary = match accepted {
            HostMessage::Accepted { screen } => screen,
            HostMessage::Error { message, .. } => bail!("Host: {message}"),
            _ => bail!("Host sent an invalid session response"),
        };
        install_client_screen(&shared, primary.clone(), true).await?;
        if let Ok(mut state) = shared.lock() {
            state.connected = true;
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
        let mut watchdog = tokio::time::interval(Duration::from_secs(3));
        let session_result: Result<()> = async {
            loop {
                tokio::select! {
                    _ = watchdog.tick() => {
                        let screens = shared.lock().map(|state| state.screens.clone()).unwrap_or_default();
                        for screen in screens {
                            let frame = screen.frame.lock().map(|frame| frame.frames).unwrap_or_default();
                            if screen.received_frames > frame && screen.received_frames > 1 {
                                screen.diagnostics.record("decoder", format!("Decoder has received {} packets but decoded {frame}", screen.received_frames));
                                if let Ok(mut state) = shared.lock() {
                                    state.message = format!("{} decoder stalled · see its Logs", screen.config.name);
                                }
                            }
                        }
                    }
                    command = commands.recv() => {
                        match command {
                            Some(Command::Disconnect) | None => break,
                            Some(Command::AddScreen) => {
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
                                send_json(&mut control_send, &ClientMessage::AddScreen(AddScreenRequest { request_id, settings: Some(settings) })).await?;
                                message(&shared, "Requesting a new host screen…");
                            }
                            Some(Command::ChangeScreenSettings(screen_id, mut settings)) => {
                                let revision = shared.lock().ok().and_then(|state| state.screens.iter().find(|screen| screen.config.screen_id == screen_id).map(|screen| screen.config.revision));
                                let Some(revision) = revision else { continue; };
                                settings.audio = false;
                                let request_id = next_request_id(&shared);
                                send_json(&mut control_send, &ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest { request_id, screen_id, revision, settings })).await?;
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
                            Some(LaneNotice::Opened(screen_id, lane)) => update_client_screen_status(&shared, screen_id, format!("{lane:?} lane opened · waiting for frames")),
                            Some(LaneNotice::FirstFrame(screen_id)) => update_client_screen_status(&shared, screen_id, "Streaming video".into()),
                            Some(LaneNotice::Failed(screen_id, lane, error)) => update_client_screen_status(&shared, screen_id, format!("{lane:?} lane failed: {error}")),
                            Some(LaneNotice::Closed(screen_id, lane)) => update_client_screen_status(&shared, screen_id, format!("{lane:?} lane closed")),
                            None => {},
                        }
                    }
                    response = receive_json::<_, HostMessage>(&mut control_recv) => {
                        match response? {
                            HostMessage::ScreenAdded { screen, .. } => {
                                match install_client_screen(&shared, screen.clone(), true).await {
                                    Ok(()) => message(&shared, format!("{} is ready · opening its independent video window", screen.name)),
                                    Err(error) => message(&shared, format!("Could not open {}: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenSettingsApplied { screen, .. } => {
                                match install_client_screen(&shared, screen.clone(), false).await {
                                    Ok(()) => message(&shared, format!("{} settings applied · stream generation {}", screen.name, screen.generation)),
                                    Err(error) => message(&shared, format!("Could not apply {} settings: {error:#}", screen.name)),
                                }
                            }
                            HostMessage::ScreenRemoved { screen_id, .. } => remove_client_screen(&shared, screen_id).await,
                            HostMessage::StreamStatus { screen_id, generation, lane, state: stream_state, message: detail } => {
                                if let Ok(mut state) = shared.lock() {
                                    if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id && screen.config.generation == generation) {
                                        screen.status = detail.clone();
                                        screen.diagnostics.record("stream", format!("{lane:?} {stream_state:?}: {detail}"));
                                    }
                                    state.message = format!("Screen {screen_id} · {lane:?} {stream_state:?}: {detail}");
                                }
                            }
                            HostMessage::Stats { screen_id, generation, frames_sent, frames_dropped, bitrate_kbps } => {
                                if let Ok(mut state) = shared.lock() {
                                    if let Some(screen) = state.screens.iter_mut().find(|screen| screen.config.screen_id == screen_id && screen.config.generation == generation) {
                                        screen.host_frames_sent = frames_sent;
                                        screen.host_frames_dropped = frames_dropped;
                                        screen.bitrate_kbps = bitrate_kbps;
                                        if frames_dropped > 0 {
                                            screen.diagnostics.record("stats", format!("Host dropped {frames_dropped} frames"));
                                        }
                                    }
                                }
                            }
                            HostMessage::Error { screen_id, message: detail, .. } => {
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
                            HostMessage::Accepted { .. } => message(&shared, "Host sent a second session acceptance; ignoring it"),
                        }
                    }
                }
            }
            Ok(())
        }.await;
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            send_json(&mut control_send, &ClientMessage::Stop),
        )
        .await;
        lane_task.abort();
        let _ = lane_task.await;
        connection.close(0u32.into(), b"viewer disconnected");
        let screens = if let Ok(mut state) = shared.lock() {
            state.connected = false;
            state.selected_screen = None;
            std::mem::take(&mut state.screens)
        } else {
            Vec::new()
        };
        for screen in screens {
            let _ = screen.video_window.close();
        }
        if session_result.is_ok() {
            message(&shared, "Disconnected");
        }
        session_result
    }

    fn update_client_screen_status(
        shared: &Arc<Mutex<State>>,
        screen_id: ScreenId,
        status: String,
    ) {
        if let Ok(mut state) = shared.lock() {
            if let Some(screen) = state
                .screens
                .iter_mut()
                .find(|screen| screen.config.screen_id == screen_id)
            {
                screen.status = status.clone();
                screen.diagnostics.record("stream", &status);
            }
        }
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
            screens: Vec::new(),
            selected_screen: None,
            next_request_id: 1,
            connected: false,
            presets,
            selected_preset: 1,
            message: "Discovering Macs…".into(),
            show_codecs: false,
            show_logs: false,
            show_screen_details: false,
            diagnostics: Arc::new(Diagnostics::new("viewer")),
            bytes: 0,
            received_frames: 0,
        }));
        let (send, receive) = mpsc::unbounded_channel();
        let network_state = state.clone();
        std::thread::spawn(move || {
            if let Err(error) = run_network(network_state.clone(), receive) {
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
                            state
                                .diagnostics
                                .record("discovery", format!("Found {}", host.name));
                            state.message =
                                format!("Found {} · enter the PIN to connect", host.name);
                            state.hosts.push(host);
                        }
                    }
                }
            }
            Err(error) => message(&browse_state, format!("Discovery unavailable: {error}")),
        });
        Application::new().run(move |cx: &mut App| {
            let controls = open_controls(cx, state.clone(), send.clone())
                .expect("Cannot open AirBridge controls");
            let _ = controls;
            state.lock().unwrap().diagnostics.record(
                "viewer",
                "Independent native video window and GPUI controls opened",
            );
            cx.activate(true);
        });
    }
}

fn main() {
    app::main();
}
