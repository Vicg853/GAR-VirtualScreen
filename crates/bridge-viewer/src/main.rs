mod linux;

mod app {
    use super::linux::{VideoDecoder, VideoFrame};
    use anyhow::{Result, bail};
    use bridge_core::{
        config::{
            Acceleration, Codec, CodecOption, Preset, VideoSettings, built_in_presets,
            load_custom_presets, save_custom_presets,
        },
        diagnostics::Diagnostics,
        discovery::{DiscoveredHost, browse},
        protocol::{
            Control, MediaKind, MediaLane, MediaState, MediaStreamHello, VERSION, receive_json,
            receive_media, send_json,
        },
        security::{authenticate_viewer, client_endpoint},
    };
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, ObjectFit,
        RenderImage, ResizeEdge, Window, WindowBounds, WindowDecorations, WindowHandle,
        WindowOptions, bounds, div, img, point, prelude::*, px, rgb, size,
    };
    use image::{Frame, RgbaImage};
    use smallvec::smallvec;
    use std::{
        cell::RefCell,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        rc::Rc,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::mpsc;

    enum Command {
        Connect(SocketAddr, String, Option<VideoSettings>),
        Disconnect,
    }

    enum LaneNotice {
        Opened(MediaLane),
        FirstFrame,
        Failed(MediaLane, String),
        Closed(MediaLane),
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
        show_logs: bool,
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

    struct ViewerView {
        state: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        commands: mpsc::UnboundedSender<Command>,
        focus: FocusHandle,
        windows: Rc<RefCell<UiWindows>>,
        rendered: bool,
    }

    #[derive(Default)]
    struct UiWindows {
        controls: Option<WindowHandle<ViewerView>>,
        video: Option<WindowHandle<VideoView>>,
    }

    struct VideoView {
        state: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        commands: mpsc::UnboundedSender<Command>,
        windows: Rc<RefCell<UiWindows>>,
        last_frame: u64,
        atlas_image: Option<Arc<RenderImage>>,
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

    impl Render for VideoView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            if !self.rendered {
                self.rendered = true;
                if let Ok(state) = self.state.lock() {
                    state.diagnostics.record(
                        "ui",
                        format!(
                            "Video window rendering at {:?}",
                            window.window_bounds().get_bounds()
                        ),
                    );
                }
            }
            let image = self.frame.lock().ok().and_then(|mut frame| {
                if let Some(bytes) = frame.pending.take() {
                    if let Some(buffer) = RgbaImage::from_raw(frame.width, frame.height, bytes) {
                        frame.image =
                            Some(Arc::new(RenderImage::new(smallvec![Frame::new(buffer)])));
                    }
                }
                frame.image.clone()
            });
            if self.atlas_image.as_ref().map(|image| image.id)
                != image.as_ref().map(|image| image.id)
            {
                if let Some(old) = self.atlas_image.take() {
                    cx.drop_image(old, Some(window));
                }
                self.atlas_image = image.clone();
            }
            let content = if let Some(image) = image {
                div()
                    .size_full()
                    .child(img(image).object_fit(ObjectFit::Contain).size_full())
            } else {
                let message = self
                    .state
                    .lock()
                    .map(|state| state.message.clone())
                    .unwrap_or_default();
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(format!("Waiting for the Mac display · {message}"))
            };
            div()
                .size_full()
                .bg(rgb(0x05080d))
                .text_color(rgb(0xf2f5fb))
                .child(content)
                .child(
                    div()
                        .id("open-controls")
                        .absolute()
                        .top_2()
                        .right_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x203149))
                        .cursor_pointer()
                        .child("Controls")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(handle) = this.windows.borrow().controls {
                                if handle
                                    .update(cx, |_, window, _| window.activate_window())
                                    .is_ok()
                                {
                                    return;
                                }
                            }
                            match open_controls(
                                cx,
                                this.state.clone(),
                                this.frame.clone(),
                                this.commands.clone(),
                                this.windows.clone(),
                            ) {
                                Ok(handle) => this.windows.borrow_mut().controls = Some(handle),
                                Err(error) => {
                                    message(&this.state, format!("Cannot reopen controls: {error}"))
                                }
                            }
                        })),
                )
        }
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
                    if label != "Save custom preset" {
                        state.message = format!("{label} updated · applies on the next connection");
                    }
                    state.diagnostics.record("settings", &state.message);
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
            let frame = self.frame.lock().unwrap();
            let decoded = frame.frames;
            let skipped = frame.skipped;
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
                                state.message = format!(
                                    "{} preset selected for the next connection",
                                    state.presets[index].name
                                );
                                state.diagnostics.record("settings", &state.message);
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
                    .id("viewer-logs-scroll")
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
                .child(logs_toggle)
                .child(logs)
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
                    .child(div().id("show-display").px_3().py_2().rounded_md().bg(rgb(0x28354b)).cursor_pointer().child("Show display window")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(handle) = this.windows.borrow().video {
                                if handle.update(cx, |_, window, _| window.activate_window()).is_ok() { return; }
                            }
                            match open_video(cx, this.state.clone(), this.frame.clone(), this.commands.clone(), this.windows.clone()) {
                                Ok(handle) => this.windows.borrow_mut().video = Some(handle),
                                Err(error) => message(&this.state, format!("Cannot reopen display: {error}")),
                            }
                        })))
                    .child(div().text_sm().text_color(rgb(0xa8b8cf)).child("PRESETS · next connection"))
                    .child(div().flex().flex_wrap().gap_2().children(presets))
                    .child(div().id("codec-menu").px_3().py_2().rounded_md().bg(rgb(0x28354b)).cursor_pointer()
                        .child(if state.show_codecs { "Video settings ▴" } else { "Video settings ▾" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Ok(mut state) = this.state.lock() { state.show_codecs = !state.show_codecs; }
                            cx.notify();
                        })))
                    .child(codec_panel)
                    .child(format!("Received {} · decoded {} · skipped {} · {:.1} MB{}", state.received_frames, decoded, skipped, state.bytes as f64 / 1_000_000.0,
                        frame_error.map(|e| format!(" · {e}")).unwrap_or_default())))
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
        frame: Arc<Mutex<VideoFrame>>,
        commands: mpsc::UnboundedSender<Command>,
        windows: Rc<RefCell<UiWindows>>,
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
                    frame,
                    commands,
                    focus,
                    windows,
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

    fn open_video(
        cx: &mut App,
        state: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        commands: mpsc::UnboundedSender<Command>,
        windows: Rc<RefCell<UiWindows>>,
    ) -> Result<WindowHandle<VideoView>> {
        let bounds = cx
            .primary_display()
            .and_then(|display| {
                let area = display.bounds();
                if area.size.width < px(1100.0) {
                    return None;
                }
                let video_size = size(
                    (area.size.width - px(460.0)).min(px(1300.0)),
                    (area.size.height - px(130.0)).min(px(850.0)),
                );
                Some(bounds(
                    point(area.origin.x + px(20.0), area.origin.y + px(70.0)),
                    video_size,
                ))
            })
            .unwrap_or_else(|| Bounds::centered(None, size(px(960.0), px(640.0)), cx));
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(320.0), px(200.0))),
                app_id: Some("airbridge-viewer".into()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("AirBridge Video");
                let view = cx.new(|_| VideoView {
                    state,
                    frame,
                    commands,
                    windows,
                    last_frame: 0,
                    atlas_image: None,
                    rendered: false,
                });
                let weak = view.downgrade();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(16))
                            .await;
                        if weak
                            .update(cx, |view, cx| {
                                let current = view
                                    .frame
                                    .lock()
                                    .map(|frame| frame.frames)
                                    .unwrap_or_default();
                                if current != view.last_frame {
                                    view.last_frame = current;
                                    cx.notify();
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
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
        frame: Arc<Mutex<VideoFrame>>,
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

    async fn receive_video_lane(
        mut media_recv: quinn::RecvStream,
        settings: VideoSettings,
        shared: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        events: mpsc::UnboundedSender<LaneNotice>,
    ) -> Result<()> {
        let diagnostics = shared.lock().unwrap().diagnostics.clone();
        let decoder = VideoDecoder::new(settings.width, settings.height, frame, diagnostics)?;
        let mut first_frame = true;
        loop {
            let (header, bytes) = receive_media(&mut media_recv).await?;
            if !matches!(header.kind, MediaKind::Video) {
                bail!("The video lane received a non-video packet");
            }
            if let Ok(mut state) = shared.lock() {
                state.bytes += bytes.len() as u64;
                state.received_frames += 1;
                if state.received_frames == 1 || state.received_frames % 120 == 0 {
                    state.diagnostics.record(
                        "viewer",
                        format!(
                            "Received {} video packets, {:.1} MB",
                            state.received_frames,
                            state.bytes as f64 / 1_000_000.0
                        ),
                    );
                }
            }
            decoder.submit(bytes).await?;
            if first_frame {
                first_frame = false;
                let _ = events.send(LaneNotice::FirstFrame);
            }
        }
    }

    async fn receive_lanes(
        connection: quinn::Connection,
        settings: VideoSettings,
        shared: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
        events: mpsc::UnboundedSender<LaneNotice>,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        let video_active = Arc::new(AtomicBool::new(false));
        loop {
            tokio::select! {
                incoming = connection.accept_uni() => {
                    let Ok(mut stream) = incoming else { break; };
                    let shared = shared.clone();
                    let frame = frame.clone();
                    let settings = settings.clone();
                    let events = events.clone();
                    let video_active = video_active.clone();
                    tasks.spawn(async move {
                        let hello = tokio::time::timeout(Duration::from_secs(5), receive_json::<_, MediaStreamHello>(&mut stream)).await;
                        match hello {
                            Ok(Ok(MediaStreamHello { version: VERSION, lane: MediaLane::Video })) => {
                                if video_active.swap(true, Ordering::AcqRel) {
                                    let _ = stream.stop(0u32.into());
                                    return (MediaLane::Video, Err(anyhow::anyhow!("Duplicate video lane")));
                                }
                                let _ = events.send(LaneNotice::Opened(MediaLane::Video));
                                let result = receive_video_lane(stream, settings, shared, frame, events).await;
                                video_active.store(false, Ordering::Release);
                                (MediaLane::Video, result)
                            }
                            Ok(Ok(MediaStreamHello { version: VERSION, lane })) => {
                                let _ = stream.stop(0u32.into());
                                (lane, Err(anyhow::anyhow!("The {lane:?} lane is not implemented yet")))
                            }
                            Ok(Ok(_)) => (MediaLane::Video, Err(anyhow::anyhow!("Incompatible media lane version"))),
                            Ok(Err(error)) => (MediaLane::Video, Err(error.context("Could not read media lane header"))),
                            Err(_) => (MediaLane::Video, Err(anyhow::anyhow!("Media lane header timed out"))),
                        }
                    });
                }
                finished = tasks.join_next(), if !tasks.is_empty() => {
                    match finished {
                        Some(Ok((lane, outcome))) => match outcome {
                            Ok(()) => { let _ = events.send(LaneNotice::Closed(lane)); },
                            Err(error) => { let _ = events.send(LaneNotice::Failed(lane, format!("{error:#}"))); },
                        },
                        Some(Err(error)) => {
                            video_active.store(false, Ordering::Release);
                            let _ = events.send(LaneNotice::Failed(MediaLane::Video, format!("Media lane task failed: {error}")));
                        }
                        None => {},
                    }
                }
            }
        }
    }

    async fn connect(
        address: SocketAddr,
        pin: String,
        settings: Option<VideoSettings>,
        shared: Arc<Mutex<State>>,
        frame: Arc<Mutex<VideoFrame>>,
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
        *frame.lock().unwrap() = VideoFrame::default();
        if let Ok(mut state) = shared.lock() {
            state.bytes = 0;
            state.received_frames = 0;
        }
        message(
            &shared,
            format!(
                "Negotiated {:?} {:?}; FFmpeg software decode",
                option.codec, option.acceleration
            ),
        );
        message(
            &shared,
            format!(
                "Connected; waiting for video {}×{} at {} fps",
                actual.width, actual.height, actual.fps
            ),
        );
        let (lane_events, mut lane_notices) = mpsc::unbounded_channel();
        let mut lane_task = tokio::spawn(receive_lanes(
            connection.clone(),
            actual,
            shared.clone(),
            frame.clone(),
            lane_events,
        ));
        let mut watchdog = tokio::time::interval(Duration::from_secs(3));
        let mut last_decoded = 0;
        let mut last_received = 0;
        let session_result: Result<()> = async {
            loop {
                tokio::select! {
                _ = watchdog.tick() => {
                    let decoded = frame.lock().map(|frame| frame.frames).unwrap_or_default();
                    let received = shared.lock().map(|state| state.received_frames).unwrap_or_default();
                    if received > last_received && decoded == last_decoded && received > 1 {
                        message(&shared, format!("Decoder stalled: received {received}, decoded {decoded}. Open Logs for FFmpeg details."));
                    }
                    last_received = received;
                    last_decoded = decoded;
                }
                command = commands.recv() => { if matches!(command, Some(Command::Disconnect) | None) { break; } }
                lane = &mut lane_task => {
                    match lane {
                        Ok(()) => bail!("The media connection closed"),
                        Err(error) => bail!("Media lane manager stopped: {error}"),
                    }
                }
                notice = lane_notices.recv() => {
                    match notice {
                        Some(LaneNotice::Opened(lane)) => message(&shared, format!("{lane:?} lane opened; waiting for frames…")),
                        Some(LaneNotice::FirstFrame) => message(&shared, "Streaming video"),
                        Some(LaneNotice::Failed(lane, error)) => message(&shared, format!("{lane:?} lane unavailable: {error}")),
                        Some(LaneNotice::Closed(lane)) => message(&shared, format!("{lane:?} lane closed")),
                        None => {},
                    }
                }
                response = receive_json::<_, Control>(&mut control_recv) => {
                    match response? {
                        Control::Error(text) => message(&shared, format!("Host: {text}")),
                        Control::MediaStatus { lane, state, message: detail } => {
                            match state {
                                MediaState::Starting | MediaState::Ready | MediaState::Failed | MediaState::Stopped =>
                                    message(&shared, format!("{lane:?}: {detail}")),
                            }
                        }
                        Control::Stop => break,
                        _ => {},
                    }
                }
                }
            }
            Ok(())
        }.await;
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            send_json(&mut control_send, &Control::Stop),
        )
        .await;
        lane_task.abort();
        let _ = lane_task.await;
        connection.close(0u32.into(), b"viewer disconnected");
        *frame.lock().unwrap() = VideoFrame::default();
        if session_result.is_ok() {
            message(&shared, "Disconnected");
        }
        session_result
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
            show_logs: false,
            diagnostics: Arc::new(Diagnostics::new("viewer")),
            bytes: 0,
            received_frames: 0,
        }));
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        let (send, receive) = mpsc::unbounded_channel();
        let network_state = state.clone();
        let network_frame = frame.clone();
        std::thread::spawn(move || {
            if let Err(error) = run_network(network_state.clone(), network_frame, receive) {
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
            let windows = Rc::new(RefCell::new(UiWindows::default()));
            let video = open_video(
                cx,
                state.clone(),
                frame.clone(),
                send.clone(),
                windows.clone(),
            )
            .expect("Cannot open AirBridge video window");
            windows.borrow_mut().video = Some(video);
            let controls = open_controls(
                cx,
                state.clone(),
                frame.clone(),
                send.clone(),
                windows.clone(),
            )
            .expect("Cannot open AirBridge controls");
            windows.borrow_mut().controls = Some(controls);
            state
                .lock()
                .unwrap()
                .diagnostics
                .record("viewer", "Video and controls windows opened");
            cx.activate(true);
        });
    }
}

fn main() {
    app::main();
}
