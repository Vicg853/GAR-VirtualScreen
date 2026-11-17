// Viewer controls. Included inside `app`. Nothing here changes the stream or
// the host: sessions are joined and left, and each screen is presented.

fn el(element: impl IntoElement) -> AnyElement {
    element.into_any_element()
}

fn stack(children: Vec<AnyElement>) -> gpui_kit::Div {
    div().flex().flex_col().gap(px(18.)).children(children)
}

fn with_state(shared: &Arc<Mutex<State>>, change: impl FnOnce(&mut State)) {
    if let Ok(mut state) = shared.lock() {
        change(&mut state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Page {
    Home,
    /// Onboarding step 1: pick a session and sign in.
    Join,
    /// Onboarding step 2: choose which of the session's screens to show here.
    Choose(u64),
    Session(u64),
    Screen(u64, ScreenId),
    /// PIN or address connection to a temporary screen.
    Connect,
}

#[derive(Default)]
struct JoinDraft {
    /// mDNS name of the host whose session is selected.
    host: Option<String>,
    by_address: bool,
    remember: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ConnectDraft {
    host: Option<String>,
    screen: u32,
    by_address: bool,
    error: Option<String>,
}

/// Interface-only audio settings; the audio engine does not exist yet.
struct AudioDraft {
    output: usize,
    volume: f32,
    mute: bool,
    buffer: String,
}

struct ViewerView {
    workspace: Arc<Mutex<Workspace>>,
    catalog: Arc<Mutex<State>>,
    page: Page,
    fields: Fields,
    join: JoinDraft,
    connect: ConnectDraft,
    confirm_forget: bool,
    audio: AudioDraft,
    /// Streams seen on the last refresh, to close the windows of ended ones.
    known: HashSet<(u64, ScreenId)>,
}

/// One-word state of a connection entry and how to draw it.
fn entry_status(state: &State) -> (&'static str, Pill) {
    if state.connected {
        ("● Connected", Pill::Live)
    } else if state.connecting {
        ("Connecting…", Pill::Neutral)
    } else if state.contract.is_some() && state.known_host().is_none() {
        ("Offline", Pill::Warn)
    } else if state.contract.is_some() && (!state.auto_connect || state.auto_suspended) {
        ("Paused", Pill::Neutral)
    } else {
        ("Disconnected", Pill::Neutral)
    }
}

fn entry_name(state: &State, id: u64) -> String {
    match &state.contract {
        Some(contract) => contract.display_name().to_owned(),
        None if !state.host_label.is_empty() => state.host_label.clone(),
        None => format!("Connection {id}"),
    }
}

fn entry_summary(state: &State) -> String {
    let host = if state.host_label.is_empty() {
        "Host"
    } else {
        &state.host_label
    };
    match &state.contract {
        Some(contract) if state.connected => format!(
            "{host} · {} screen{} · {} viewing",
            contract.screens.len(),
            if contract.screens.len() == 1 { "" } else { "s" },
            state.screens.iter().filter(|s| !s.terminal).count()
        ),
        Some(_) if state.known_host().is_none() => "Saved · host not on network".into(),
        Some(_) if !state.auto_connect => "Saved · auto-connect paused on this device".into(),
        Some(_) => format!("Saved · {}", state.message),
        None if state.connected => format!(
            "PIN connection · {}",
            state
                .screens
                .first()
                .map(|s| format!("{} {}", s.config.slot, s.config.name))
                .unwrap_or_else(|| "no screen".into())
        ),
        None => format!("PIN connection · {}", state.message),
    }
}

fn incoming_summary(config: &ScreenConfig) -> String {
    let settings = &config.settings;
    format!(
        "{}×{} · {} fps · {}{} · {} Mbps",
        settings.width,
        settings.height,
        settings.fps,
        config.codec.codec.label(),
        if config.codec.codec == Codec::H264 {
            match settings.h264_profile {
                bridge_core::config::H264Profile::Baseline => " Baseline",
                bridge_core::config::H264Profile::Main => " Main",
                bridge_core::config::H264Profile::High => " High",
            }
        } else {
            ""
        },
        settings.bitrate_kbps / 1000
    )
}

/// The monitors a stream can be shown on, as the native window will find them.
fn display_targets(cx: &App) -> Vec<DisplayTarget> {
    let primary = cx.primary_display().map(|display| display.id());
    cx.displays()
        .iter()
        .enumerate()
        .map(|(index, display)| {
            let bounds = display.bounds();
            DisplayTarget {
                x: f32::from(bounds.origin.x).round() as i32,
                y: f32::from(bounds.origin.y).round() as i32,
                label: format!(
                    "Display {} — {}×{}{}",
                    index + 1,
                    f32::from(bounds.size.width).round() as i32,
                    f32::from(bounds.size.height).round() as i32,
                    if Some(display.id()) == primary {
                        " (primary)"
                    } else {
                        ""
                    }
                ),
            }
        })
        .collect()
}

/// Change how one slot is presented on this device, remember it, and apply it
/// to the open window. The host is not told and the stream is not touched.
fn present(entry: &LocalScreen, id: ScreenId, change: impl FnOnce(&mut ViewerDisplay)) {
    with_state(&entry.state, |state| {
        if let Some(slot) = state.preferences.screen_mut(id) {
            change(&mut slot.display);
        }
        save_preferences(state);
    });
    let _ = entry.commands.try_send(Command::Present(id));
}

/// Show or pause a session screen on this device.
fn set_visible(entry: &LocalScreen, id: ScreenId, visible: bool) {
    let connected = entry
        .state
        .lock()
        .map(|mut state| {
            if let Some(slot) = state.preferences.screen_mut(id) {
                slot.visible = visible;
            }
            if !state.connected {
                if visible {
                    state.removed_screens.remove(&id);
                } else {
                    state.removed_screens.insert(id);
                }
            }
            save_preferences(&mut state);
            state.connected
        })
        .unwrap_or(false);
    if connected {
        let _ = entry.commands.try_send(if visible {
            Command::AttachScreen(id)
        } else {
            Command::RemoveScreen(id)
        });
    }
}

fn parse_address(text: &str) -> Option<SocketAddr> {
    let text = text.trim();
    text.parse::<SocketAddr>().ok().or_else(|| {
        text.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, bridge_core::protocol::PORT))
    })
}

impl ViewerView {
    fn new(workspace: Arc<Mutex<Workspace>>, catalog: Arc<Mutex<State>>) -> Self {
        let first_launch = workspace
            .lock()
            .map(|ws| ws.entries.is_empty())
            .unwrap_or(true);
        Self {
            workspace,
            catalog,
            page: if first_launch { Page::Join } else { Page::Home },
            fields: Fields::default(),
            join: JoinDraft {
                remember: true,
                ..Default::default()
            },
            connect: ConnectDraft::default(),
            confirm_forget: false,
            audio: AudioDraft {
                output: 0,
                volume: 80.,
                mute: false,
                buffer: "40".into(),
            },
            known: HashSet::new(),
        }
    }

    fn entries(&self) -> Vec<LocalScreen> {
        self.workspace
            .lock()
            .map(|ws| ws.entries.clone())
            .unwrap_or_default()
    }

    fn entry(&self, id: u64) -> Option<LocalScreen> {
        self.workspace
            .lock()
            .ok()
            .and_then(|ws| ws.entries.iter().find(|entry| entry.id == id).cloned())
    }

    fn go(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        self.confirm_forget = false;
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut ws) = self.workspace.lock() {
            ws.retiring.retain(|entry| {
                let finished = entry
                    .thread
                    .lock()
                    .map(|t| t.as_ref().is_none_or(|t| t.is_finished()))
                    .unwrap_or(true);
                if finished {
                    if let Some(thread) = entry.thread.lock().ok().and_then(|mut t| t.take()) {
                        let _ = thread.join();
                    }
                }
                !finished
            });
        }
        let mut live = HashSet::new();
        for entry in self.entries() {
            if let Ok(mut state) = entry.state.lock() {
                for screen in &mut state.screens {
                    live.insert((entry.id, screen.config.screen_id));
                    if screen.stats_view.open {
                        screen.stats_view.refresh(&screen.telemetry);
                    }
                }
            }
        }
        for (entry, screen) in self.known.difference(&live) {
            bridge_ui::close_auxiliary(&format!("viewer-logs:{entry}:{screen}"), cx);
        }
        self.known = live;
        // Leave pages whose subject is gone.
        match self.page.clone() {
            Page::Screen(entry, screen)
                if !self.known.contains(&(entry, screen)) =>
            {
                self.page = if self.entry(entry).is_some() {
                    Page::Session(entry)
                } else {
                    Page::Home
                };
            }
            Page::Session(entry) | Page::Choose(entry) if self.entry(entry).is_none() => {
                self.page = Page::Home;
            }
            _ => {}
        }
        cx.notify();
    }

    fn inspect_stats(&mut self, entry: &LocalScreen, id: ScreenId, cx: &mut Context<Self>) {
        if let Ok(mut state) = entry.state.lock() {
            let Some(index) = state.screens.iter().position(|s| s.config.screen_id == id) else {
                return;
            };
            if !state.screens[index].stats_view.open {
                if let Err(error) = entry.commands.try_send(Command::Diagnostics(id, true)) {
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
        let shared = entry.state.clone();
        let read = shared.clone();
        let commands = entry.commands.clone();
        let source = bridge_ui::InspectionSource::new(
            move || {
                let mut state = read.lock().ok()?;
                let screen = state
                    .screens
                    .iter_mut()
                    .find(|s| s.config.screen_id == id)?;
                if !screen.stats_view.open {
                    return None;
                }
                let telemetry = screen.telemetry.clone();
                screen.stats_view.refresh(&telemetry);
                let title = format!(
                    "{} ({}) · {:?} {:?} · generation {}",
                    screen.config.name,
                    screen.config.slot,
                    screen.config.codec.codec,
                    screen.config.codec.acceleration,
                    screen.config.generation
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
                    if let Some(screen) =
                        state.screens.iter_mut().find(|s| s.config.screen_id == id)
                    {
                        if !screen.stats_view.open {
                            return;
                        }
                        let telemetry = screen.telemetry.clone();
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
                                screen.stats_view.frozen = if screen.stats_view.frozen.is_some() {
                                    None
                                } else {
                                    Some(telemetry.snapshot())
                                };
                                screen.stats_view.freeze_after = None;
                            }
                            bridge_ui::InspectionAction::Burst => {
                                screen.stats_view.frozen = None;
                                screen.stats_view.freeze_after = Some(
                                    telemetry.snapshot().averages[Stage::Capture as usize].count
                                        + 20,
                                );
                            }
                        }
                    } else {
                        return;
                    }
                    if matches!(action, bridge_ui::InspectionAction::Close) {
                        if let Err(error) = commands.try_send(Command::Diagnostics(id, false)) {
                            state.message = format!("Could not disable statistics: {error}");
                            state.diagnostics.record("stats", &state.message);
                        }
                    }
                    if matches!(action, bridge_ui::InspectionAction::Burst) {
                        let _ = commands.try_send(Command::Burst(id));
                    }
                }
            },
        );
        if let Err(error) =
            bridge_ui::open_inspector(format!("viewer:{}:{id}", entry.id), source.clone(), cx)
        {
            source.close();
            message(&entry.state, format!("Cannot open statistics: {error:#}"));
        }
    }

    fn open_logs(&mut self, entry: &LocalScreen, id: ScreenId, cx: &mut Context<Self>) {
        let target = entry.state.lock().ok().and_then(|state| {
            let screen = state.screen(id)?;
            Some((
                format!(
                    "AirBridge · Logs — {} ({})",
                    screen.config.name, screen.config.slot
                ),
                screen.diagnostics.clone(),
            ))
        });
        if let Some((title, diagnostics)) = target {
            if let Err(error) = bridge_ui::logs::open_logs(
                format!("viewer-logs:{}:{id}", entry.id),
                title,
                diagnostics,
                cx,
            ) {
                message(&entry.state, format!("Cannot open logs: {error:#}"));
            }
        }
    }

    /// Create a connection entry and dial. Returns the entry's ID.
    fn start_connection(
        &mut self,
        address: SocketAddr,
        username: String,
        secret: String,
        remember: bool,
        screen: u32,
    ) -> Option<u64> {
        let entry = add_local_session(&self.workspace, &self.catalog, None)?;
        with_state(&entry.state, |state| {
            state.username = username;
            state.pin = secret.clone();
            state.remember = remember;
            state.auto_connect = remember;
            state.auto_suspended = false;
            state.target_screen = screen;
            state.connecting = true;
        });
        let _ = entry
            .commands
            .try_send(Command::Connect(address, secret, false));
        Some(entry.id)
    }

    // ───────────────────────── Onboarding ─────────────────────────

    fn join(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let hosts = self
            .catalog
            .lock()
            .map(|state| state.hosts.clone())
            .unwrap_or_default();
        let has_entries = !self.entries().is_empty();
        let mut found = Vec::new();
        for host in &hosts {
            for session in &host.sessions {
                let selected = self.join.host.as_deref() == Some(host.name.as_str())
                    && self.fields.text("join:session", cx).as_deref()
                        == Some(session.username.as_str());
                let pick_view = view.clone();
                let host_name = host.name.clone();
                let username = session.username.clone();
                found.push(el(kit::item(
                    None,
                    session.name.clone(),
                    format!(
                        "{} · {} screen{} · password",
                        host.label,
                        session.screens,
                        if session.screens == 1 { "" } else { "s" }
                    ),
                    [if selected {
                        el(kit::pill("Selected", Pill::Live, cx))
                    } else {
                        el(kit::tool_button(
                            SharedString::from(format!("join-pick:{}:{username}", host.name)),
                            "Select",
                        )
                        .on_click(move |_, window, cx| {
                            let _ = pick_view.update(cx, |this, cx| {
                                this.join.host = Some(host_name.clone());
                                this.join.by_address = false;
                                this.join.error = None;
                                this.fields.set_text("join:session", &username, window, cx);
                                cx.notify();
                            });
                        }))
                    }],
                    cx,
                )));
            }
            if host.sessions.is_empty() && !host.screens.is_empty() {
                let pin_view = view.clone();
                let host_name = host.name.clone();
                found.push(el(kit::item(
                    None,
                    host.label.clone(),
                    "No session · PIN per screen",
                    [el(kit::tool_button(
                        SharedString::from(format!("join-pin:{}", host.name)),
                        "Use PIN",
                    )
                    .on_click(move |_, _, cx| {
                        let _ = pin_view.update(cx, |this, cx| {
                            this.connect = ConnectDraft {
                                host: Some(host_name.clone()),
                                ..Default::default()
                            };
                            this.go(Page::Connect, cx);
                        });
                    }))],
                    cx,
                )));
            }
        }
        if found.is_empty() {
            found.push(el(div().px(px(14.)).py(px(12.)).child(kit::hint(
                "Looking for hosts on this network… You can also connect by address.",
                cx,
            ))));
        }

        let session = self.fields.input(
            "join:session",
            "",
            InputSpec {
                placeholder: "Session name",
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, _, _, _, _| this.join.error = None,
        );
        let password = self.fields.input(
            "join:password",
            "",
            InputSpec {
                placeholder: "Password",
                masked: true,
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, _, _, _, _| this.join.error = None,
        );
        let mut login = Vec::new();
        if self.join.by_address {
            let address = self.fields.input(
                "join:address",
                "",
                InputSpec {
                    placeholder: "192.168.1.10 or IP:port",
                    ..Default::default()
                },
                window,
                cx,
                |this: &mut Self, _, _, _, _| this.join.error = None,
            );
            login.push(el(kit::row("Host address", Input::new(&address).small(), cx)));
        }
        login.push(el(kit::row("Session", Input::new(&session).small(), cx)));
        login.push(el(kit::row(
            "Password",
            Input::new(&password).small().mask_toggle(),
            cx,
        )));
        let remember_view = view.clone();
        login.push(el(kit::row_wide(
            "Remember on this device",
            None,
            kit::toggle(
                "join-remember",
                self.join.remember,
                "Remember on this device",
                move |value, _, cx| {
                    let _ = remember_view.update(cx, |this, cx| {
                        this.join.remember = value;
                        cx.notify();
                    });
                },
            ),
            cx,
        )));

        let by_address = self.join.by_address;
        let mut page = vec![
            el(div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().flex().child(kit::pill("Step 1 of 2", Pill::Neutral, cx)))
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Join a session"),
                )
                .child(
                    kit::hint(
                        "Use the same session name and password as the host. Next time, this device connects on its own.",
                        cx,
                    )
                    .text_size(px(13.)),
                )),
            el(kit::section(
                kit::section_label("Found on this network", cx),
                kit::card(found, cx),
            )),
            el(kit::card(login, cx)),
        ];
        if let Some(error) = &self.join.error {
            page.push(el(div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(error.clone())));
        }
        page.push(el(div()
            .flex()
            .gap_2()
            .when(has_entries, |row| {
                row.child(
                    Button::new("join-cancel")
                        .label("Cancel")
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| this.go(Page::Home, cx))),
                )
            })
            .child(
                Button::new("join-by-address")
                    .label(if by_address {
                        "Use a found host"
                    } else {
                        "Connect by address…"
                    })
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.join.by_address = !this.join.by_address;
                        this.join.error = None;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("join-continue")
                    .label("Continue")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.submit_join(window, cx))),
            )));
        page
    }

    fn submit_join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let username = self
            .fields
            .text("join:session", cx)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let password = self.fields.text("join:password", cx).unwrap_or_default();
        if let Err(error) = bridge_core::sessions::validate_login(&username, &password) {
            self.join.error = Some(error.to_string());
            cx.notify();
            return;
        }
        let address = if self.join.by_address {
            self.fields
                .text("join:address", cx)
                .and_then(|text| parse_address(&text))
        } else {
            self.catalog.lock().ok().and_then(|state| {
                state
                    .hosts
                    .iter()
                    .filter(|host| {
                        self.join.host.as_deref() == Some(host.name.as_str())
                            || (self.join.host.is_none()
                                && host.sessions.iter().any(|s| s.username == username))
                    })
                    .flat_map(|host| host.addresses.first().copied())
                    .next()
            })
        };
        let Some(address) = address else {
            self.join.error = Some(
                if self.join.by_address {
                    "Enter the host's IP address"
                } else {
                    "Select a session found on this network, or connect by address"
                }
                .into(),
            );
            cx.notify();
            return;
        };
        match self.start_connection(address, username, password, self.join.remember, 0) {
            Some(entry) => {
                self.fields.set_text("join:password", "", window, cx);
                self.go(Page::Choose(entry), cx);
            }
            None => {
                self.join.error = Some("The session limit on this device is reached".into());
                cx.notify();
            }
        }
    }

    fn choose(&mut self, entry: &LocalScreen, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let state = entry.state.lock().unwrap().clone();
        let targets = display_targets(cx);
        let mut page = vec![el(div()
            .flex()
            .flex_col()
            .gap_2()
            .child(div().flex().child(kit::pill("Step 2 of 2", Pill::Neutral, cx)))
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Choose screens to show here"),
            )
            .child(
                kit::hint(
                    "Paused screens stay in the session but aren’t streamed to this device. Change it any time.",
                    cx,
                )
                .text_size(px(13.)),
            ))];
        let back_entry = entry.clone();
        let back = Button::new("choose-back")
            .label("Back")
            .outline()
            .on_click(cx.listener(move |this, _, _, cx| {
                remove_local_screen(&this.workspace, back_entry.id);
                this.go(Page::Join, cx);
            }));
        if !state.connected {
            page.push(el(if state.connecting {
                kit::hint(state.message.clone(), cx).text_size(px(13.))
            } else {
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().danger)
                    .child(state.message.clone())
            }));
            page.push(el(div().flex().child(back)));
            return page;
        }
        let mut rows = Vec::new();
        for saved in state.contract.iter().flat_map(|c| c.screens.iter()) {
            let id = saved.screen_id;
            let visible = state.preferences.visible(id);
            let slot = state
                .contract
                .as_ref()
                .and_then(|c| c.slot_label(id))
                .unwrap_or_default();
            let display = state.display(id);
            let toggle_entry = entry.clone();
            let target_entry = entry.clone();
            let options = targets.clone();
            rows.push(el(kit::item(
                Some(el(kit::monitor_icon(false, !visible, cx))),
                saved.name.clone(),
                if visible {
                    format!(
                        "{slot} · {}×{} · {} fps",
                        saved.settings.width, saved.settings.height, saved.settings.fps
                    )
                } else {
                    format!("{slot} · paused on this device")
                },
                [
                    el(kit::select(
                        ("choose-target", id as usize),
                        std::iter::once(Choice::new("Automatic", display.target.is_none()))
                            .chain(targets.iter().map(|target| {
                                Choice::new(
                                    target.label.clone(),
                                    display.target.as_ref() == Some(target),
                                )
                            }))
                            .collect(),
                        move |index, _, _| {
                            let target = index.checked_sub(1).and_then(|i| options.get(i)).cloned();
                            present(&target_entry, id, |display| display.target = target);
                        },
                    )),
                    el(kit::toggle(
                        ("choose-visible", id as usize),
                        visible,
                        format!("Show {}", saved.name),
                        move |visible, _, _| set_visible(&toggle_entry, id, visible),
                    )),
                ],
                cx,
            )));
        }
        if rows.is_empty() {
            rows.push(el(div().px(px(14.)).py(px(12.)).child(kit::hint(
                "This session has no screens yet. They appear here when the host adds them.",
                cx,
            ))));
        }
        page.push(el(kit::card(rows, cx)));
        let auto_state = entry.state.clone();
        page.push(el(kit::card(
            [el(kit::row_wide(
                "Auto-connect on this device",
                None,
                kit::toggle(
                    "choose-auto",
                    state.auto_connect,
                    "Auto-connect",
                    move |value, _, _| {
                        with_state(&auto_state, |state| {
                            state.auto_connect = value;
                            state.auto_suspended = false;
                            if let Err(e) = save_viewer_session(state) {
                                state.message = format!("Session save failed: {e}");
                            }
                        })
                    },
                )
                .disabled(!state.remember),
                cx,
            ))],
            cx,
        )));
        let id = entry.id;
        page.push(el(div()
            .flex()
            .gap_2()
            .child(back)
            .child(div().flex_1())
            .child(
                Button::new("choose-start")
                    .label("Start viewing")
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| this.go(Page::Session(id), cx))),
            )));
        page
    }

    // ───────────────────────── Home ─────────────────────────

    fn home(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let entries = self.entries();
        let mut rows = Vec::new();
        for entry in &entries {
            let state = entry.state.lock().unwrap();
            let (status, kind) = entry_status(&state);
            let id = entry.id;
            let open_view = view.clone();
            rows.push(el(kit::item_link(
                ("local-session", id as usize),
                None,
                entry_name(&state, id),
                entry_summary(&state),
                move |_, cx| {
                    let _ = open_view.update(cx, |this, cx| this.go(Page::Session(id), cx));
                },
                [el(kit::pill(status, kind, cx))],
                cx,
            )));
        }
        if rows.is_empty() {
            rows.push(el(div().px(px(14.)).py(px(12.)).child(kit::hint(
                "No sessions yet. Add one to connect to a host.",
                cx,
            ))));
        }
        let hosts = self
            .catalog
            .lock()
            .map(|state| state.hosts.clone())
            .unwrap_or_default();
        let mut others = Vec::new();
        for host in hosts.iter().filter(|host| !host.screens.is_empty()) {
            let pin_view = view.clone();
            let host_name = host.name.clone();
            others.push(el(kit::item(
                None,
                host.label.clone(),
                format!(
                    "{} shared screen{} · PIN required",
                    host.screens.len(),
                    if host.screens.len() == 1 { "" } else { "s" }
                ),
                [el(kit::tool_button(
                    SharedString::from(format!("home-pin:{}", host.name)),
                    "Connect with PIN",
                )
                .on_click(move |_, _, cx| {
                    let _ = pin_view.update(cx, |this, cx| {
                        this.connect = ConnectDraft {
                            host: Some(host_name.clone()),
                            ..Default::default()
                        };
                        this.go(Page::Connect, cx);
                    });
                }))],
                cx,
            )));
        }
        others.push(el(div().px(px(12.)).py(px(10.)).child(
            Button::new("home-by-address")
                .label("Connect by address…")
                .outline()
                .w_full()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.connect = ConnectDraft {
                        by_address: true,
                        screen: 1,
                        ..Default::default()
                    };
                    this.go(Page::Connect, cx);
                })),
        )));
        vec![
            el(kit::section(
                kit::section_label("Host sessions", cx),
                kit::card(rows, cx),
            )),
            el(Button::new("add-host-session")
                .icon(IconName::Plus)
                .label("Add host session")
                .primary()
                .w_full()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.join = JoinDraft {
                        remember: true,
                        ..Default::default()
                    };
                    this.go(Page::Join, cx);
                }))),
            el(kit::section(
                kit::section_label("Other hosts on network", cx),
                kit::card(others, cx),
            )),
        ]
    }

    // ───────────────────────── PIN / address ─────────────────────────

    fn connect_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let hosts = self
            .catalog
            .lock()
            .map(|state| state.hosts.clone())
            .unwrap_or_default();
        let hosts = hosts
            .into_iter()
            .filter(|host| !host.screens.is_empty())
            .collect::<Vec<_>>();
        let by_address = self.connect.by_address || hosts.is_empty();
        let mut rows = Vec::new();
        if by_address {
            let address = self.fields.input(
                "connect:address",
                "",
                InputSpec {
                    placeholder: "192.168.1.10 or IP:port",
                    ..Default::default()
                },
                window,
                cx,
                |this: &mut Self, _, _, _, _| this.connect.error = None,
            );
            let screen = self.fields.input(
                "connect:screen",
                &self.connect.screen.max(1).to_string(),
                InputSpec {
                    digits: Some(3),
                    ..Default::default()
                },
                window,
                cx,
                |this: &mut Self, text, _, _, _| {
                    if let Ok(value) = text.parse::<u32>() {
                        this.connect.screen = value;
                    }
                },
            );
            rows.push(el(kit::row("Host address", Input::new(&address).small(), cx)));
            rows.push(el(kit::row_with_hint(
                "Screen slot",
                div().w(px(90.)).child(Input::new(&screen).small()),
                "The number after “T-” shown on the host for that screen.",
                cx,
            )));
        } else {
            let selected = hosts
                .iter()
                .find(|host| self.connect.host.as_deref() == Some(host.name.as_str()))
                .unwrap_or(&hosts[0])
                .clone();
            if !selected
                .screens
                .iter()
                .any(|screen| screen.screen_id == self.connect.screen)
            {
                self.connect.screen = selected.screens[0].screen_id;
            }
            let names = hosts.iter().map(|host| host.name.clone()).collect::<Vec<_>>();
            let host_view = view.clone();
            rows.push(el(kit::row(
                "Host",
                kit::select(
                    "connect-host",
                    hosts
                        .iter()
                        .map(|host| Choice::new(host.label.clone(), host.name == selected.name))
                        .collect(),
                    move |index, _, cx| {
                        let name = names.get(index).cloned();
                        let _ = host_view.update(cx, |this, cx| {
                            this.connect.host = name;
                            this.connect.screen = 0;
                            cx.notify();
                        });
                    },
                ),
                cx,
            )));
            let ids = selected
                .screens
                .iter()
                .map(|screen| screen.screen_id)
                .collect::<Vec<_>>();
            let screen_view = view.clone();
            rows.push(el(kit::row(
                "Screen",
                kit::select(
                    "connect-screen",
                    selected
                        .screens
                        .iter()
                        .map(|screen| {
                            Choice::new(
                                screen.name.clone(),
                                screen.screen_id == self.connect.screen,
                            )
                        })
                        .collect(),
                    move |index, _, cx| {
                        let id = ids.get(index).copied().unwrap_or(0);
                        let _ = screen_view.update(cx, |this, cx| {
                            this.connect.screen = id;
                            cx.notify();
                        });
                    },
                ),
                cx,
            )));
        }
        let pin = self.fields.input(
            "connect:pin",
            "",
            InputSpec {
                placeholder: "Six digits shown on the host",
                masked: true,
                digits: Some(6),
            },
            window,
            cx,
            |this: &mut Self, _, _, _, _| this.connect.error = None,
        );
        rows.push(el(kit::row(
            "Screen PIN",
            Input::new(&pin).small().mask_toggle(),
            cx,
        )));
        let mut page = vec![
            el(kit::hint(
                "A PIN connects to one temporary screen on the host. Nothing is remembered.",
                cx,
            )
            .text_size(px(13.))),
            el(kit::card(rows, cx)),
        ];
        if let Some(error) = &self.connect.error {
            page.push(el(div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(error.clone())));
        }
        let host_names = hosts;
        page.push(el(div()
            .flex()
            .gap_2()
            .child(
                Button::new("connect-cancel")
                    .label("Cancel")
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let page = if this.entries().is_empty() {
                            Page::Join
                        } else {
                            Page::Home
                        };
                        this.go(page, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("connect")
                    .label("Connect")
                    .primary()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let pin = this.fields.text("connect:pin", cx).unwrap_or_default();
                        if bridge_core::security::validate_pin(&pin).is_err() {
                            this.connect.error = Some("Enter the six-digit screen PIN".into());
                            cx.notify();
                            return;
                        }
                        let address = if by_address {
                            this.fields
                                .text("connect:address", cx)
                                .and_then(|text| parse_address(&text))
                        } else {
                            host_names
                                .iter()
                                .find(|host| {
                                    this.connect.host.as_deref() == Some(host.name.as_str())
                                })
                                .or(host_names.first())
                                .and_then(|host| host.addresses.first().copied())
                        };
                        let Some(address) = address else {
                            this.connect.error = Some("Enter the host's IP address".into());
                            cx.notify();
                            return;
                        };
                        let screen = this.connect.screen.max(1);
                        match this.start_connection(address, String::new(), pin, false, screen) {
                            Some(entry) => {
                                this.fields.set_text("connect:pin", "", window, cx);
                                this.go(Page::Session(entry), cx);
                            }
                            None => {
                                this.connect.error =
                                    Some("The session limit on this device is reached".into());
                                cx.notify();
                            }
                        }
                    })),
            )));
        page
    }

    // ───────────────────────── Session ─────────────────────────

    fn session(
        &mut self,
        entry: &LocalScreen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let state = entry.state.lock().unwrap().clone();
        let entry_id = entry.id;
        let mut page = Vec::new();
        if !state.connected && !state.message.is_empty() {
            page.push(el(kit::hint(state.message.clone(), cx)));
        }

        if state.contract.is_some() {
            let auto_state = entry.state.clone();
            let remember_state = entry.state.clone();
            let awake_state = entry.state.clone();
            let policy = state.viewer_lock_policy();
            let awake_hint = match policy {
                bridge_core::sessions::ViewerLockPolicy::ViewerChoice
                    if !state.power_notice.is_empty() =>
                {
                    state.power_notice.clone()
                }
                bridge_core::sessions::ViewerLockPolicy::ViewerChoice => {
                    "Desktop lock policy still applies".into()
                }
                policy => format!("Set by the host: {}", policy.label()),
            };
            page.push(el(kit::section(
                kit::section_label("Session", cx),
                kit::card(
                    [
                        el(kit::row_wide(
                            "Auto-connect on this device",
                            Some("Pausing here doesn’t affect the host’s setting.".into()),
                            kit::toggle(
                                "auto-connect-session",
                                state.auto_connect,
                                "Auto-connect",
                                move |value, _, _| {
                                    with_state(&auto_state, |state| {
                                        state.auto_connect = value;
                                        state.auto_suspended = false;
                                        if let Err(e) = save_viewer_session(state) {
                                            state.message = format!("Session save failed: {e}");
                                        } else if let Some(saved) = &mut state.remembered {
                                            saved.auto_connect = value;
                                        }
                                    })
                                },
                            )
                            .disabled(!state.remember),
                            cx,
                        )),
                        el(kit::row_wide(
                            "Remember credentials",
                            None,
                            kit::toggle(
                                "remember-session",
                                state.remember,
                                "Remember credentials",
                                move |value, _, _| {
                                    with_state(&remember_state, |state| {
                                        state.remember = value;
                                        state.auto_connect = value;
                                        state.auto_suspended = false;
                                        if let Err(e) = save_viewer_session(state) {
                                            state.message = format!("Session save failed: {e}");
                                        } else {
                                            state.remembered =
                                                value.then(|| saved_viewer_session(state)).flatten();
                                        }
                                    })
                                },
                            ),
                            cx,
                        )),
                        el(kit::row_wide(
                            "Keep viewer awake",
                            Some(awake_hint.into()),
                            kit::toggle(
                                "viewer-keep-awake",
                                state.effective_keep_awake(),
                                "Keep viewer awake",
                                move |value, _, _| {
                                    with_state(&awake_state, |state| {
                                        state.preferences.keep_awake = value;
                                        save_preferences(state);
                                    })
                                },
                            )
                            .disabled(
                                policy != bridge_core::sessions::ViewerLockPolicy::ViewerChoice,
                            ),
                            cx,
                        )),
                    ],
                    cx,
                ),
            )));
        }

        // Screens: every slot of the session, or the one PIN screen.
        struct Slot {
            id: ScreenId,
            name: String,
            label: String,
            session: bool,
        }
        let slots = match &state.contract {
            Some(contract) => contract
                .screens
                .iter()
                .map(|saved| Slot {
                    id: saved.screen_id,
                    name: saved.name.clone(),
                    label: contract.slot_label(saved.screen_id).unwrap_or_default(),
                    session: true,
                })
                .collect::<Vec<_>>(),
            None => state
                .screens
                .iter()
                .map(|screen| Slot {
                    id: screen.config.screen_id,
                    name: screen.config.name.clone(),
                    label: screen.config.slot.clone(),
                    session: false,
                })
                .collect(),
        };
        let mut rows = Vec::new();
        for slot in &slots {
            let id = slot.id;
            let attached = state.screen(id).filter(|screen| !screen.removing);
            let visible = state.preferences.visible(id);
            let display = state.display(id);
            let (detail, status) = match attached {
                Some(screen) if screen.terminal => (
                    format!("{} · {}", slot.label, screen.status),
                    kit::pill("Stopped", Pill::Warn, cx),
                ),
                Some(screen) => (
                    format!(
                        "{} · {} · {}",
                        slot.label,
                        screen.config.codec.codec.label(),
                        display
                            .target
                            .as_ref()
                            .map(|target| format!(
                                "on {}",
                                target.label.split(" — ").next().unwrap_or("display")
                            ))
                            .unwrap_or_else(|| screen.status.clone())
                    ),
                    kit::pill("● Showing", Pill::Live, cx),
                ),
                None if !visible => (
                    format!("{} · paused on this device", slot.label),
                    kit::pill("Paused", Pill::Neutral, cx),
                ),
                None if state.connected => (
                    format!("{} · not created on the host right now", slot.label),
                    kit::pill("Unavailable", Pill::Neutral, cx),
                ),
                None => (
                    format!("{} · shown when connected", slot.label),
                    kit::pill("Waiting", Pill::Neutral, cx),
                ),
            };
            let mut trailing = vec![el(status)];
            if slot.session {
                let toggle_entry = entry.clone();
                trailing.push(el(kit::toggle(
                    ("show-screen", id as usize),
                    visible,
                    format!("Show {}", slot.name),
                    move |visible, _, _| set_visible(&toggle_entry, id, visible),
                )));
            }
            rows.push(el(kit::item(
                Some(el(kit::monitor_icon(false, attached.is_none(), cx))),
                slot.name.clone(),
                detail,
                trailing,
                cx,
            )));
            if let Some(screen) = attached {
                let window_handle = screen.video_window.clone();
                let show_state = entry.state.clone();
                let retry = entry.commands.clone();
                let detach = entry.clone();
                let can_restart = !screen.terminal
                    || screen
                        .frame
                        .lock()
                        .map(|frame| frame.cleanup_complete)
                        .unwrap_or(false);
                rows.push(el(div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .px(px(12.))
                    .pb(px(10.))
                    .child(
                        kit::tool_button(("configure-screen", id as usize), "Configure").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.go(Page::Screen(entry_id, id), cx)
                            }),
                        ),
                    )
                    .child(
                        kit::tool_button(("show-display", id as usize), "Show window")
                            .disabled(screen.terminal)
                            .on_click(move |_, window, _| {
                                if let Err(error) = window_handle.show() {
                                    message(&show_state, format!("Cannot show display: {error:#}"));
                                }
                                window.refresh();
                            }),
                    )
                    .child(
                        kit::tool_button(
                            ("retry-stream", id as usize),
                            if screen.terminal {
                                "Retry stream"
                            } else {
                                "Restart stream"
                            },
                        )
                        .disabled(!can_restart)
                        .on_click(move |_, _, _| {
                            let _ = retry.try_send(Command::RetryScreen(id));
                        }),
                    )
                    .child(
                        kit::tool_button(("remove-stream", id as usize), "Detach")
                            .danger()
                            .on_click(move |_, _, _| {
                                if detach
                                    .state
                                    .lock()
                                    .map(|state| state.contract.is_some())
                                    .unwrap_or(false)
                                {
                                    set_visible(&detach, id, false);
                                } else {
                                    let _ = detach.commands.try_send(Command::RemoveScreen(id));
                                }
                            }),
                    )));
            }
        }
        if rows.is_empty() {
            rows.push(el(div().px(px(14.)).py(px(12.)).child(kit::hint(
                if state.contract.is_some() {
                    "This session has no screens yet."
                } else {
                    "No screen attached."
                },
                cx,
            ))));
        }
        page.push(el(div()
            .flex()
            .flex_col()
            .gap_2()
            .child(kit::section_label("Screens", cx))
            .child(kit::card(rows, cx))
            .child(kit::hint(
                "Restart stream reconnects this device only. Stream settings are changed on the host.",
                cx,
            ))));

        // A saved session without a remembered key needs its password again.
        let needs_password = state.contract.is_some()
            && state.remembered.is_none()
            && state.authenticated_key.is_none();
        let password_key = format!("session:{entry_id}:password");
        if !state.connected && !state.connecting && needs_password {
            let password = self.fields.input(
                &password_key,
                "",
                InputSpec {
                    placeholder: "Session password",
                    masked: true,
                    ..Default::default()
                },
                window,
                cx,
                |_: &mut Self, _, _, _, _| {},
            );
            page.push(el(kit::card(
                [el(kit::row(
                    "Password",
                    Input::new(&password).small().mask_toggle(),
                    cx,
                ))],
                cx,
            )));
        }

        let online = state.connected || state.connecting;
        let address = state
            .known_host()
            .and_then(|host| host.addresses.first().copied())
            .or(state.last_endpoint);
        let link_entry = entry.clone();
        let confirm = self.confirm_forget;
        page.push(el(div()
            .flex()
            .gap_2()
            .pt(px(14.))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(if online {
                Button::new("disconnect")
                    .label(if state.connecting {
                        "Cancel connection"
                    } else {
                        "Disconnect"
                    })
                    .danger()
                    .outline()
                    .on_click(move |_, window, _| {
                        with_state(&link_entry.state, |state| state.auto_suspended = true);
                        let _ = link_entry.commands.try_send(Command::Disconnect);
                        window.refresh();
                    })
            } else {
                Button::new("connect")
                    .label("Connect")
                    .primary()
                    .disabled(address.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let Some(address) = address else { return };
                        let typed = this.fields.text(&password_key, cx).unwrap_or_default();
                        this.fields.set_text(&password_key, "", window, cx);
                        let secret = link_entry
                            .state
                            .lock()
                            .map(|mut state| {
                                state.auto_suspended = false;
                                if !typed.is_empty() || state.pin.is_empty() {
                                    state.pin = typed;
                                }
                                state.connecting = true;
                                state.pin.clone()
                            })
                            .unwrap_or_default();
                        let _ = link_entry
                            .commands
                            .try_send(Command::Connect(address, secret, false));
                        cx.notify();
                    }))
            })
            .child(div().flex_1())
            .child(
                Button::new("forget-session")
                    .label(match (state.contract.is_some(), confirm) {
                        (true, false) => "Forget session…",
                        (true, true) => "Confirm forget",
                        (false, _) => "Remove",
                    })
                    .danger()
                    .outline()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let session = this
                            .entry(entry_id)
                            .and_then(|entry| {
                                entry.state.lock().ok().map(|s| s.contract.is_some())
                            })
                            .unwrap_or(false);
                        if session && !this.confirm_forget {
                            this.confirm_forget = true;
                            cx.notify();
                            return;
                        }
                        // One action: drop the connection, the session and its saved login.
                        remove_local_screen(&this.workspace, entry_id);
                        let prefix = format!("session:{entry_id}:");
                        this.fields.retain(|key| !key.starts_with(&prefix));
                        this.go(Page::Home, cx);
                    })),
            )));
        page
    }

    // ───────────────────────── Screen display config ─────────────────────────

    fn screen_page(
        &mut self,
        entry: &LocalScreen,
        id: ScreenId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let state = entry.state.lock().unwrap().clone();
        let Some(screen) = state.screen(id).cloned() else {
            return vec![];
        };
        let display = state.display(id);
        let targets = display_targets(cx);
        let choices = |labels: &[&'static str], selected: usize| {
            labels
                .iter()
                .enumerate()
                .map(|(index, label)| Choice::new(*label, index == selected))
                .collect::<Vec<_>>()
        };

        let target_entry = entry.clone();
        let target_options = targets.clone();
        let mode_entry = entry.clone();
        let scaling_entry = entry.clone();
        let filter_entry = entry.clone();
        let rotation_entry = entry.clone();
        let vsync_entry = entry.clone();
        let decoder_entry = entry.clone();
        let jitter = self.fields.input(
            &format!("display:{}:{id}:jitter", entry.id),
            &display.jitter_frames.to_string(),
            InputSpec {
                digits: Some(1),
                ..Default::default()
            },
            window,
            cx,
            |_: &mut Self, _, _, _, _| {},
        );
        const MODES: [WindowMode; 3] =
            [WindowMode::Fullscreen, WindowMode::Window, WindowMode::Borderless];
        const SCALINGS: [Scaling; 4] =
            [Scaling::Fit, Scaling::Fill, Scaling::Stretch, Scaling::Native];
        const FILTERS: [UpscaleFilter; 3] = [
            UpscaleFilter::Bilinear,
            UpscaleFilter::Lanczos,
            UpscaleFilter::Nearest,
        ];
        let position = |found: Option<usize>| found.unwrap_or(0);
        let presentation = kit::card(
            [
                el(kit::row(
                    "Show on",
                    kit::select(
                        "display-target",
                        std::iter::once(Choice::new("Automatic", display.target.is_none()))
                            .chain(targets.iter().map(|target| {
                                Choice::new(
                                    target.label.clone(),
                                    display.target.as_ref() == Some(target),
                                )
                            }))
                            .collect(),
                        move |index, _, _| {
                            let target = index
                                .checked_sub(1)
                                .and_then(|i| target_options.get(i))
                                .cloned();
                            present(&target_entry, id, |display| display.target = target);
                        },
                    ),
                    cx,
                )),
                el(kit::row(
                    "Window mode",
                    kit::segmented(
                        "display-mode",
                        choices(
                            &["Fullscreen", "Window", "Borderless"],
                            position(MODES.iter().position(|m| *m == display.window_mode)),
                        ),
                        move |index, _, _| {
                            present(&mode_entry, id, |display| display.window_mode = MODES[index])
                        },
                        cx,
                    ),
                    cx,
                )),
                el(kit::row(
                    "Scaling",
                    kit::segmented(
                        "display-scaling",
                        choices(
                            &["Fit", "Fill", "Stretch", "1:1"],
                            position(SCALINGS.iter().position(|s| *s == display.scaling)),
                        ),
                        move |index, _, _| {
                            present(&scaling_entry, id, |display| {
                                display.scaling = SCALINGS[index]
                            })
                        },
                        cx,
                    ),
                    cx,
                )),
                el(kit::row(
                    "Upscale filter",
                    kit::select(
                        "display-filter",
                        vec![
                            Choice::new("Bilinear", display.filter == UpscaleFilter::Bilinear),
                            Choice::new("Lanczos", display.filter == UpscaleFilter::Lanczos)
                                .unavailable(Some("not in this renderer yet")),
                            Choice::new(
                                "Nearest (pixel-exact)",
                                display.filter == UpscaleFilter::Nearest,
                            ),
                        ],
                        move |index, _, _| {
                            present(&filter_entry, id, |display| display.filter = FILTERS[index])
                        },
                    ),
                    cx,
                )),
                el(kit::row(
                    "Rotation",
                    kit::segmented(
                        "display-rotation",
                        choices(
                            &["0°", "90°", "180°", "270°"],
                            usize::from(display.rotation / 90),
                        ),
                        move |index, _, _| {
                            present(&rotation_entry, id, |display| {
                                display.rotation = index as u16 * 90
                            })
                        },
                        cx,
                    ),
                    cx,
                )),
                el(kit::row_wide(
                    "VSync presentation",
                    Some("Off = lowest latency, may tear.".into()),
                    kit::toggle(
                        "display-vsync",
                        display.vsync,
                        "VSync presentation",
                        move |value, _, _| {
                            present(&vsync_entry, id, |display| display.vsync = value)
                        },
                    ),
                    cx,
                )),
                el(div()
                    .id("display-jitter")
                    .opacity(0.5)
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(
                            "Frames are presented as they arrive; buffering is not in this renderer yet",
                        )
                        .build(window, cx)
                    })
                    .child(kit::row(
                        "Jitter buffer",
                        kit::inline()
                            .child(
                                div()
                                    .w(px(90.))
                                    .child(Input::new(&jitter).small().disabled(true)),
                            )
                            .child(kit::unit("frames", cx)),
                        cx,
                    ))),
                el(kit::row_with_hint(
                    "Decoder",
                    kit::select(
                        "display-decoder",
                        vec![
                            Choice::new(
                                "Hardware (auto)",
                                display.decoder == DecoderChoice::Hardware,
                            ),
                            Choice::new("Software", display.decoder == DecoderChoice::Software),
                        ],
                        move |index, _, _| {
                            present(&decoder_entry, id, |display| {
                                display.decoder = if index == 0 {
                                    DecoderChoice::Hardware
                                } else {
                                    DecoderChoice::Software
                                }
                            })
                        },
                    ),
                    "Takes effect when the stream restarts.",
                    cx,
                )),
            ],
            cx,
        );

        // Audio: interface only, nothing behind it yet.
        macro_rules! set {
            ($field:ident) => {{
                let view = view.clone();
                move |value, _: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| {
                        this.audio.$field = value;
                        cx.notify();
                    });
                }
            }};
        }
        let volume = self.fields.slider(
            "audio:volume",
            self.audio.volume,
            0.,
            100.,
            1.,
            window,
            cx,
            |this: &mut Self, value, _, _| this.audio.volume = value,
        );
        let buffer = self.fields.input(
            "audio:buffer",
            &self.audio.buffer.clone(),
            InputSpec {
                digits: Some(4),
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, text, _, _, _| this.audio.buffer = text,
        );
        let audio = kit::card(
            [
                el(kit::row(
                    "Output",
                    kit::select(
                        "audio-output",
                        choices(&["System default"], self.audio.output),
                        set!(output),
                    ),
                    cx,
                )),
                el(kit::row("Volume", Slider::new(&volume), cx)),
                el(kit::row_wide(
                    "Mute",
                    None,
                    kit::toggle("audio-mute", self.audio.mute, "Mute", set!(mute)),
                    cx,
                )),
                el(kit::row(
                    "Audio buffer",
                    kit::inline()
                        .child(div().w(px(90.)).child(Input::new(&buffer).small()))
                        .child(kit::unit("ms", cx)),
                    cx,
                )),
            ],
            cx,
        );

        let mut page = vec![el(kit::card(
            [el(div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(kit::hint(
                            format!("Incoming · {}", incoming_summary(&screen.config)),
                            cx,
                        )),
                )
                .child(kit::pill("Set on host", Pill::Neutral, cx)))],
            cx,
        ))];
        let error = screen.frame.lock().ok().and_then(|frame| frame.error.clone());
        if let Some(error) = error {
            page.push(el(div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(error)));
        }
        page.push(el(kit::section(
            kit::section_label("Display on this device", cx),
            presentation,
        )));
        page.push(el(kit::dimmed(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(kit::section_header(
                    "Audio on this device",
                    kit::pill("Not available yet", Pill::Neutral, cx),
                    cx,
                ))
                .child(audio),
        )));
        page
    }
}

impl Render for ViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let catalog = self.catalog.clone();
        let workspace = self.workspace.clone();
        let gear = el(kit::icon_button("viewer-settings", IconName::Settings, "App settings")
            .on_click(move |_, _, cx| open_settings(catalog.clone(), workspace.clone(), cx)));
        let back = |page: Page, label: &'static str, cx: &mut Context<Self>| {
            kit::icon_button("viewer-back", IconName::ChevronLeft, label)
                .on_click(cx.listener(move |this, _, _, cx| this.go(page.clone(), cx)))
        };
        let (actions, crumb, page) = match self.page.clone() {
            Page::Screen(entry_id, id)
                if self
                    .entry(entry_id)
                    .is_some_and(|e| e.state.lock().is_ok_and(|s| s.screen(id).is_some())) =>
            {
                let entry = self.entry(entry_id).unwrap();
                let (name, detail, status) = {
                    let state = entry.state.lock().unwrap();
                    let screen = state.screen(id).unwrap();
                    (
                        screen.config.name.clone(),
                        format!(
                            "{} · {}",
                            if state.host_label.is_empty() {
                                "Host"
                            } else {
                                &state.host_label
                            },
                            screen.config.slot
                        ),
                        if screen.terminal {
                            kit::pill("Stopped", Pill::Warn, cx)
                        } else if screen.status == "Streaming video" {
                            kit::pill("● Viewing", Pill::Live, cx)
                        } else {
                            kit::pill("Starting…", Pill::Neutral, cx)
                        },
                    )
                };
                let crumb = kit::crumb(
                    back(Page::Session(entry_id), "Back to session", cx),
                    name,
                    detail,
                    Some(el(status)),
                    cx,
                );
                let stats_entry = entry.clone();
                let logs_entry = entry.clone();
                let actions = vec![
                    el(Button::new("viewer-stats")
                        .icon(IconName::ChartPie)
                        .label("Stats")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.inspect_stats(&stats_entry, id, cx)
                        }))),
                    el(Button::new("viewer-logs")
                        .icon(IconName::FileText)
                        .label("Logs")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_logs(&logs_entry, id, cx)
                        }))),
                    gear,
                ];
                let page = self.screen_page(&entry, id, window, cx);
                (actions, Some(el(crumb)), page)
            }
            Page::Session(entry_id) | Page::Screen(entry_id, _)
                if self.entry(entry_id).is_some() =>
            {
                let entry = self.entry(entry_id).unwrap();
                let (name, host, status, kind) = {
                    let state = entry.state.lock().unwrap();
                    let (status, kind) = entry_status(&state);
                    (
                        entry_name(&state, entry_id),
                        if state.contract.is_some() {
                            state.host_label.clone()
                        } else {
                            "PIN connection".into()
                        },
                        status,
                        kind,
                    )
                };
                let crumb = kit::crumb(
                    back(Page::Home, "Back to sessions", cx),
                    name,
                    host,
                    Some(el(kit::pill(status, kind, cx))),
                    cx,
                );
                let page = self.session(&entry, window, cx);
                (vec![gear], Some(el(crumb)), page)
            }
            Page::Choose(entry_id) if self.entry(entry_id).is_some() => {
                let entry = self.entry(entry_id).unwrap();
                (vec![], None, self.choose(&entry, cx))
            }
            Page::Join => (vec![], None, self.join(window, cx)),
            Page::Connect => {
                let crumb = kit::crumb(
                    back(Page::Home, "Back", cx),
                    "Connect with PIN",
                    "",
                    None,
                    cx,
                );
                (vec![gear], Some(el(crumb)), self.connect_page(window, cx))
            }
            _ => (vec![gear], None, self.home(cx)),
        };
        bridge_ui::shell("AirBridge · Viewer", actions, crumb, stack(page), cx)
    }
}

// ───────────────────────── App settings window ─────────────────────────

struct SettingsView {
    catalog: Arc<Mutex<State>>,
    workspace: Arc<Mutex<Workspace>>,
    device: String,
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (hosts, viewer_id, diagnostics) = self
            .catalog
            .lock()
            .map(|state| {
                (
                    state.hosts.len(),
                    state.stable_viewer.clone(),
                    state.diagnostics.clone(),
                )
            })
            .unwrap_or_default();
        let entries = self
            .workspace
            .lock()
            .map(|ws| ws.entries.clone())
            .unwrap_or_default();
        let saved = entries
            .iter()
            .filter(|entry| entry.state.lock().is_ok_and(|s| s.remembered.is_some()))
            .count();
        let pause_entries = entries.clone();
        let all_paused = saved > 0
            && entries.iter().all(|entry| {
                entry
                    .state
                    .lock()
                    .is_ok_and(|s| s.remembered.is_none() || !s.auto_connect)
            });
        let page = vec![
            el(kit::section(
                kit::section_label("This device", cx),
                kit::card(
                    [
                        el(kit::row(
                            "Name",
                            div().text_size(px(13.)).child(self.device.clone()),
                            cx,
                        )),
                        el(kit::row_with_hint(
                            "Viewer ID",
                            div()
                                .text_size(px(13.))
                                .child(viewer_id.chars().take(8).collect::<String>()),
                            "Hosts recognise this device by it.",
                            cx,
                        )),
                        el(kit::row_wide(
                            "Video display",
                            Some(
                                if cfg!(target_os = "linux") {
                                    "Native GPU window per screen"
                                } else {
                                    "The native video window is implemented for Linux only"
                                }
                                .into(),
                            ),
                            kit::pill(
                                if cfg!(target_os = "linux") {
                                    "Available"
                                } else {
                                    "Unavailable"
                                },
                                Pill::Neutral,
                                cx,
                            ),
                            cx,
                        )),
                    ],
                    cx,
                ),
            )),
            el(kit::section(
                kit::section_label("Sessions", cx),
                kit::card(
                    [el(kit::row_wide(
                        "Pause auto-connect for every session",
                        Some(
                            format!(
                                "{saved} saved session{} on this device.",
                                if saved == 1 { "" } else { "s" }
                            )
                            .into(),
                        ),
                        kit::toggle("settings-pause-all", all_paused, "Pause auto-connect", {
                            move |pause, _, _| {
                                for entry in &pause_entries {
                                    with_state(&entry.state, |state| {
                                        if state.remembered.is_some() {
                                            state.auto_connect = !pause;
                                            state.auto_suspended = false;
                                            if let Err(e) = save_viewer_session(state) {
                                                state.message =
                                                    format!("Session save failed: {e}");
                                            } else if let Some(saved) = &mut state.remembered {
                                                saved.auto_connect = !pause;
                                            }
                                        }
                                    });
                                }
                            }
                        })
                        .disabled(saved == 0),
                        cx,
                    ))],
                    cx,
                ),
            )),
            el(kit::section(
                kit::section_label("Network", cx),
                kit::card(
                    [
                        el(kit::row_wide(
                            "Hosts found on this network",
                            None,
                            kit::pill(hosts.to_string(), Pill::Neutral, cx),
                            cx,
                        )),
                        el(kit::row_wide(
                            "Application log",
                            Some("Discovery and connection events of this app.".into()),
                            kit::tool_button("settings-app-log", "Open").on_click(
                                move |_, _, cx| {
                                    let _ = bridge_ui::logs::open_logs(
                                        "viewer-app-logs".into(),
                                        "AirBridge · Logs — Viewer".into(),
                                        diagnostics.clone(),
                                        cx,
                                    );
                                },
                            ),
                            cx,
                        )),
                    ],
                    cx,
                ),
            )),
        ];
        bridge_ui::shell("AirBridge · Settings", vec![], None, stack(page), cx)
    }
}

fn open_settings(catalog: Arc<Mutex<State>>, workspace: Arc<Mutex<Workspace>>, cx: &mut App) {
    let shared = catalog.clone();
    let result = bridge_ui::open_auxiliary(
        "viewer-settings".into(),
        size(px(480.), px(520.)),
        cx,
        move |window, cx| {
            window.set_window_title("AirBridge · Settings");
            let view = cx.new(|_| SettingsView {
                catalog,
                workspace,
                device: bridge_core::config::device_name(),
            });
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    if weak.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
            view
        },
    );
    if let Err(error) = result {
        message(&shared, format!("Cannot open settings: {error:#}"));
    }
}

fn open_controls(
    cx: &mut App,
    workspace: Arc<Mutex<Workspace>>,
    state: Arc<Mutex<State>>,
) -> Result<AnyWindowHandle> {
    let widget_size = size(px(480.0), px(720.0));
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
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(400.0), px(320.0))),
            app_id: Some("airbridge-viewer".into()),
            ..TitleBar::window_options()
        },
        cx,
        |window, cx| {
            window.set_window_title("AirBridge · Viewer");
            let view = cx.new(|_| ViewerView::new(workspace, state));
            let controls_id = window.window_handle().window_id();
            cx.on_window_closed(move |cx, closed_id| {
                if closed_id == controls_id {
                    cx.quit();
                }
            })
            .detach();
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    if weak.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
            view
        },
    )?;
    Ok(handle.into())
}
