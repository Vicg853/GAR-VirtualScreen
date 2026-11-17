// Host controls. Included inside `app`; rendering reads snapshots and writes
// drafts, the service applies them.

/// Edits settle for this long before the stream is rebuilt with them, so
/// dragging a slider restarts the stream once, not on every step.
const APPLY_DELAY: Duration = Duration::from_millis(350);

fn el(element: impl IntoElement) -> AnyElement {
    element.into_any_element()
}

fn stack(children: Vec<AnyElement>) -> gpui_kit::Div {
    div().flex().flex_col().gap(px(18.)).children(children)
}

/// Change one screen's settings from a control; applied after `APPLY_DELAY`.
fn edit(shared: &Arc<Mutex<State>>, id: ScreenId, change: impl FnOnce(&mut VideoSettings)) {
    if let Ok(mut state) = shared.lock() {
        if let Some(screen) = state.screen_mut(id).filter(|screen| !screen.removing) {
            change(&mut screen.settings);
            screen.edited = Some(Instant::now());
        }
    }
}

fn with_state(shared: &Arc<Mutex<State>>, change: impl FnOnce(&mut State)) {
    if let Ok(mut state) = shared.lock() {
        change(&mut state);
    }
}

fn stream_summary(settings: &VideoSettings) -> String {
    format!(
        "{}×{} · {} fps · {}",
        settings.width,
        settings.height,
        settings.fps,
        settings.codec().label()
    )
}

fn pin_text(pin: &str) -> String {
    match pin.len() {
        6 => format!("{} {}", &pin[..3], &pin[3..]),
        _ => pin.to_owned(),
    }
}

fn phase_pill(screen: &HostScreen, cx: &App) -> AnyElement {
    el(if screen.removing {
        kit::pill("Removing…", Pill::Neutral, cx)
    } else {
        match screen.phase {
            Phase::Streaming => kit::pill("● Streaming", Pill::Live, cx),
            Phase::Error => kit::pill("Error", Pill::Warn, cx),
            Phase::Waiting if screen.edited.is_some() => kit::pill("Applying…", Pill::Neutral, cx),
            Phase::Waiting => kit::pill("Waiting for viewer", Pill::Neutral, cx),
        }
    })
}

/// Interface-only audio settings. Audio streaming stays disabled; nothing here
/// reaches the stream until that engine exists.
struct AudioDraft {
    enabled: bool,
    source: usize,
    codec: usize,
    bitrate: String,
    sample_rate: usize,
    channels: usize,
    offset: String,
    options_open: bool,
    opus_application: usize,
    opus_frame: usize,
    opus_vbr: bool,
    opus_fec: bool,
    aac_profile: usize,
    aac_mode: usize,
    pcm_depth: usize,
}

impl Default for AudioDraft {
    fn default() -> Self {
        Self {
            enabled: false,
            source: 0,
            codec: 0,
            bitrate: "128".into(),
            sample_rate: 1,
            channels: 1,
            offset: "0".into(),
            options_open: true,
            opus_application: 0,
            opus_frame: 0,
            opus_vbr: true,
            opus_fec: true,
            aac_profile: 0,
            aac_mode: 0,
            pcm_depth: 0,
        }
    }
}

/// What the "New session" page has collected so far.
#[derive(Default)]
struct SessionDraft {
    auto_connect: bool,
    /// Temporary screens to leave out of the new session.
    excluded: HashSet<ScreenId>,
    /// Additional screens to create in it.
    extra: Vec<(u32, u32)>,
    resolution: usize,
    error: Option<String>,
}

struct HostView {
    state: Arc<Mutex<State>>,
    fields: Fields,
    draft: SessionDraft,
    /// Screens whose resolution row shows the custom width × height inputs.
    custom_resolution: HashSet<ScreenId>,
    codec_open: bool,
    audio: AudioDraft,
    /// Screens seen on the last refresh, to close the windows of removed ones.
    known: HashSet<ScreenId>,
}

impl HostView {
    fn new(state: Arc<Mutex<State>>) -> Self {
        Self {
            state,
            fields: Fields::default(),
            draft: SessionDraft::default(),
            custom_resolution: HashSet::new(),
            codec_open: true,
            audio: AudioDraft::default(),
            known: HashSet::new(),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let mut live = HashSet::new();
        if let Ok(mut state) = self.state.lock() {
            if let Err(error) = persist_sessions(&mut state) {
                state.alert(format!("Session save failed: {error}"));
            }
            let serving = state.session_commands.is_some();
            let mut due = Vec::new();
            for screen in &mut state.screens {
                live.insert(screen.screen_id);
                normalize_stats_target(screen);
                if screen.stats_view.open {
                    let telemetry = stats_telemetry(screen);
                    screen.stats_view.refresh(&telemetry);
                }
                if screen.edited.is_some_and(|at| at.elapsed() >= APPLY_DELAY) {
                    screen.edited = None;
                    if !screen.removing && screen.settings.validate().is_ok() {
                        screen.diagnostics.record(
                            "settings",
                            format!("Applying {}", stream_summary(&screen.settings)),
                        );
                        if serving {
                            screen.status = "Applying updated settings…".into();
                        }
                        due.push((screen.screen_id, screen.settings.clone()));
                    }
                }
            }
            for (id, settings) in due {
                state.send(HostUiCommand::ChangeSettings(id, settings));
            }
        }
        for removed in self.known.difference(&live) {
            bridge_ui::close_auxiliary(&format!("host-logs:{removed}"), cx);
            let prefix = format!("screen:{removed}:");
            self.fields.retain(|key| !key.starts_with(&prefix));
        }
        self.known = live;
        cx.notify();
    }

    fn inspect_stats(&mut self, id: ScreenId, cx: &mut Context<Self>) {
        if let Ok(mut state) = self.state.lock() {
            let Some(index) = state.screens.iter().position(|s| s.screen_id == id) else {
                return;
            };
            if !state.screens[index].stats_view.open {
                if let Some(Err(error)) = state
                    .session_commands
                    .as_ref()
                    .map(|commands| commands.try_send(HostUiCommand::Diagnostics(id, true)))
                {
                    state.alert(format!("Could not enable statistics: {error}"));
                    cx.notify();
                    return;
                }
                state.screens[index].stats_view.reset();
                state.screens[index].stats_view.open = true;
            }
        } else {
            return;
        }
        let shared = self.state.clone();
        let read = shared.clone();

        let source = bridge_ui::InspectionSource::new(
            move || {
                let mut state = read.lock().ok()?;
                let slot = state.screen(id).map(|screen| slot_label(&state, screen))?;
                let screen = state.screen_mut(id)?;
                normalize_stats_target(screen);
                if !screen.stats_view.open {
                    return None;
                }
                let telemetry = stats_telemetry(screen);
                screen.stats_view.refresh(&telemetry);
                let title = format!(
                    "{} ({slot}) · {} · generation {}",
                    screen.name,
                    screen
                        .viewers
                        .iter()
                        .find(|peer| Some(peer.id) == screen.stats_peer)
                        .map(|peer| peer.name.as_str())
                        .unwrap_or("shared source"),
                    screen.generation,
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
                    if let Some(screen) = state.screen_mut(id) {
                        if !screen.stats_view.open {
                            return;
                        }
                        let telemetry = stats_telemetry(screen);
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
                        if let Some(Err(error)) = state
                            .session_commands
                            .as_ref()
                            .map(|commands| commands.try_send(HostUiCommand::Diagnostics(id, false)))
                        {
                            state.alert(format!("Could not disable statistics: {error}"));
                        }
                    }
                    if matches!(action, bridge_ui::InspectionAction::Burst) {
                        state.send(HostUiCommand::Burst(id));
                    }
                }
            },
        );
        if let Err(error) = bridge_ui::open_inspector(format!("host:{id}"), source.clone(), cx) {
            source.close();
            set_alert(&self.state, format!("Cannot open statistics: {error:#}"));
        }
    }

    fn open_logs(&mut self, id: ScreenId, cx: &mut Context<Self>) {
        let target = self.state.lock().ok().and_then(|state| {
            let screen = state.screen(id)?;
            Some((
                format!("AirBridge · Logs — {} ({})", screen.name, slot_label(&state, screen)),
                screen.diagnostics.clone(),
            ))
        });
        if let Some((title, diagnostics)) = target {
            if let Err(error) =
                bridge_ui::logs::open_logs(format!("host-logs:{id}"), title, diagnostics, cx)
            {
                set_alert(&self.state, format!("Cannot open logs: {error:#}"));
            }
        }
    }

    // ───────────────────────── Home ─────────────────────────

    fn screen_row(&self, state: &State, screen: &HostScreen, cx: &App) -> AnyElement {
        let id = screen.screen_id;
        let shared = self.state.clone();
        let temporary = state.sessions.owner(id).is_none();
        let detail = if temporary {
            stream_summary(&screen.settings)
        } else {
            format!(
                "{} · {}",
                slot_label(state, screen),
                stream_summary(&screen.settings)
            )
        };
        let mut trailing = Vec::new();
        if !temporary || screen.phase != Phase::Waiting || screen.removing {
            trailing.push(phase_pill(screen, cx));
        }
        if temporary && !screen.removing {
            let sessions = state
                .sessions
                .sessions
                .iter()
                .map(|s| {
                    (
                        s.contract.session_id.clone(),
                        s.contract.display_name().to_owned(),
                    )
                })
                .collect::<Vec<_>>();
            let move_state = self.state.clone();
            let targets = sessions.clone();
            trailing.push(el(Button::new(("host-move", id as usize))
                .label("Move to session")
                .outline()
                .small()
                .disabled(sessions.is_empty())
                .dropdown_menu(move |mut menu, _, _| {
                    for (session_id, name) in targets.clone() {
                        let move_state = move_state.clone();
                        menu = menu.item(
                            PopupMenuItem::new(name).on_click(
                                move |_, window, _| {
                                    with_state(&move_state, |state| {
                                        move_to_session(state, id, &session_id)
                                    });
                                    window.refresh();
                                },
                            ),
                        );
                    }
                    menu
                })));
        }
        el(kit::item_link(
            ("host-screen", id as usize),
            Some(el(kit::monitor_icon(temporary, false, cx))),
            screen.name.clone(),
            detail,
            move |_, _| with_state(&shared, |state| state.page = Page::Screen(id)),
            trailing,
            cx,
        ))
    }

    fn session_card(
        &self,
        state: &State,
        session: &bridge_core::sessions::HostSession,
        cx: &App,
    ) -> AnyElement {
        let session_id = session.contract.session_id.clone();
        let expanded = state.expanded.contains(&session_id);
        let saved = session.contract.screens.len();
        let viewers = session
            .contract
            .screens
            .iter()
            .filter_map(|saved| state.screen(saved.screen_id))
            .flat_map(|screen| screen.viewers.iter().map(|peer| peer.name.clone()))
            .collect::<std::collections::BTreeSet<_>>();
        let summary = if !session.active {
            format!(
                "{saved} screen{} · inactive — screens not created",
                if saved == 1 { "" } else { "s" }
            )
        } else {
            format!(
                "Signed in · {saved} saved screen{} · {}",
                if saved == 1 { "" } else { "s" },
                match viewers.len() {
                    0 => "viewer offline".to_owned(),
                    1 => format!("Viewer “{}” online", viewers.iter().next().unwrap()),
                    count => format!("{count} viewers online"),
                }
            )
        };
        let toggle_state = self.state.clone();
        let toggle_id = session_id.clone();
        let expand_state = self.state.clone();
        let expand_id = session_id.clone();
        let mut rows = vec![el(kit::item(
            None,
            session.contract.display_name().to_owned(),
            summary,
            [
                el(if session.active {
                    kit::pill("Active", Pill::Live, cx)
                } else {
                    kit::pill("Off", Pill::Neutral, cx)
                }),
                el(kit::toggle(
                    SharedString::from(format!("session-active:{session_id}")),
                    session.active,
                    format!("{} active", session.contract.display_name()),
                    move |active, _, _| set_session_active(&toggle_state, &toggle_id, active),
                )),
                el(kit::icon_button(
                    SharedString::from(format!("session-expand:{session_id}")),
                    if expanded {
                        IconName::ChevronUp
                    } else {
                        IconName::ChevronDown
                    },
                    if expanded { "Collapse" } else { "Expand" },
                )
                .on_click(move |_, window, _| {
                    with_state(&expand_state, |state| {
                        if !state.expanded.remove(&expand_id) {
                            state.expanded.insert(expand_id.clone());
                        }
                    });
                    window.refresh();
                })),
            ],
            cx,
        )
        .px(px(14.))
        .py(px(12.)))];
        if expanded {
            let auto_state = self.state.clone();
            let auto_id = session_id.clone();
            rows.push(el(kit::row_wide(
                "Auto-connect",
                Some(
                    "When both apps are open on the same network, connect automatically. Only affects this device."
                        .into(),
                ),
                kit::toggle(
                    SharedString::from(format!("session-auto:{session_id}")),
                    session.auto_connect,
                    "Auto-connect",
                    move |enabled, _, _| {
                        with_state(&auto_state, |state| {
                            if let Some(session) = state.sessions.session_mut(&auto_id) {
                                session.auto_connect = enabled;
                            }
                        })
                    },
                ),
                cx,
            )));
            rows.push(el(div()
                .px(px(14.))
                .pt(px(10.))
                .pb(px(4.))
                .child(kit::section_label("Screens · remembered", cx))));
            for saved in &session.contract.screens {
                match state.screen(saved.screen_id) {
                    Some(screen) => rows.push(self.screen_row(state, screen, cx)),
                    None => rows.push(el(kit::item(
                        Some(el(kit::monitor_icon(false, true, cx))),
                        saved.name.clone(),
                        format!(
                            "{} · {}",
                            session
                                .contract
                                .slot_label(saved.screen_id)
                                .unwrap_or_default(),
                            stream_summary(&saved.settings)
                        ),
                        [el(kit::pill("Not created", Pill::Neutral, cx))],
                        cx,
                    ))),
                }
            }
            let add_state = self.state.clone();
            let add_id = session_id.clone();
            rows.push(el(div().px(px(12.)).py(px(10.)).child(
                Button::new(SharedString::from(format!("session-add-screen:{session_id}")))
                    .icon(IconName::Plus)
                    .label("Add screen to session")
                    .outline()
                    .w_full()
                    .disabled(!session.active)
                    .on_click(move |_, window, _| {
                        with_state(&add_state, |state| {
                            if let Some(id) =
                                add_host_screen(state, Some(&add_id), VideoSettings::default())
                            {
                                state.page = Page::Screen(id);
                            }
                        });
                        window.refresh();
                    }),
            )));
        }
        el(kit::card(rows, cx))
    }

    fn home(&mut self, state: &State, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let temporary = state
            .screens
            .iter()
            .filter(|screen| state.sessions.owner(screen.screen_id).is_none())
            .collect::<Vec<_>>();
        let new_state = self.state.clone();
        let new_session = Button::new("host-new-session")
            .icon(IconName::Plus)
            .label("New session")
            .outline()
            .small()
            .on_click(move |_, _, cx| {
                let excluded = HashSet::new();
                let _ = view.update(cx, |this, cx| {
                    this.draft = SessionDraft {
                        auto_connect: true,
                        excluded,
                        resolution: 1,
                        ..Default::default()
                    };
                    this.fields.retain(|key| !key.starts_with("draft:"));
                    cx.notify();
                });
                with_state(&new_state, |state| state.page = Page::NewSession);
            });
        let mut sections = vec![el(kit::section_header(
            format!("Sessions · {}", state.sessions.sessions.len()),
            new_session,
            cx,
        ))];
        if state.sessions.sessions.is_empty() {
            sections.push(el(kit::hint(
                "A session is a shared login: its screens, their positions and settings are remembered, and a signed-in viewer on the same network connects on its own.",
                cx,
            )));
        }
        for session in &state.sessions.sessions {
            sections.push(self.session_card(state, session, cx));
        }
        let add_state = self.state.clone();
        let mut rows = temporary
            .iter()
            .map(|screen| self.screen_row(state, screen, cx))
            .collect::<Vec<_>>();
        rows.push(el(div().px(px(12.)).py(px(10.)).child(
            Button::new("host-add-temporary")
                .icon(IconName::Plus)
                .label("Add temporary screen")
                .outline()
                .w_full()
                .on_click(move |_, window, _| {
                    with_state(&add_state, |state| {
                        add_host_screen(state, None, VideoSettings::default());
                    });
                    window.refresh();
                }),
        )));
        sections.push(el(kit::section(
            kit::section_header(
                "Temporary screens",
                kit::pill("Forgotten on quit", Pill::Warn, cx),
                cx,
            ),
            kit::card(rows, cx),
        )));
        if !state.power_notice.is_empty() {
            sections.push(el(kit::hint(state.power_notice.clone(), cx)));
        }
        // The session cards sit directly under their heading.
        let heading = sections.remove(0);
        let cards = sections
            .drain(..state.sessions.sessions.len().max(1).min(sections.len() - 1))
            .collect::<Vec<_>>();
        let mut page = vec![el(div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading)
            .children(cards))];
        page.extend(sections);
        page
    }

    // ───────────────────────── New session ─────────────────────────

    fn new_session(
        &mut self,
        state: &State,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let view = cx.entity().downgrade();
        let name = self.fields.input(
            "draft:name",
            "",
            InputSpec {
                placeholder: "e.g. studio-desk",
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, _, _, _, _| this.draft.error = None,
        );
        let password = self.fields.input(
            "draft:password",
            "",
            InputSpec {
                placeholder: "Optional",
                masked: true,
                ..Default::default()
            },
            window,
            cx,
            |_: &mut Self, _, _, _, _| {},
        );
        let auto_view = view.clone();
        let login = kit::card(
            [
                el(kit::row("Session name", Input::new(&name).small(), cx)),
                el(kit::row_with_hint(
                    "Password",
                    Input::new(&password).small().mask_toggle(),
                    "Leave empty to allow any viewer on this network.",
                    cx,
                )),
                el(kit::row_wide(
                    "Auto-connect on this device",
                    None,
                    kit::toggle(
                        "draft-auto",
                        self.draft.auto_connect,
                        "Auto-connect",
                        move |enabled, _, cx| {
                            let _ = auto_view.update(cx, |this, cx| {
                                this.draft.auto_connect = enabled;
                                cx.notify();
                            });
                        },
                    ),
                    cx,
                )),
            ],
            cx,
        );

        let mut screens = Vec::new();
        for screen in state
            .screens
            .iter()
            .filter(|s| state.sessions.owner(s.screen_id).is_none() && !s.removing)
        {
            let id = screen.screen_id;
            let include_view = view.clone();
            screens.push(el(kit::item(
                Some(el(kit::monitor_icon(true, false, cx))),
                screen.name.clone(),
                format!(
                    "{}×{} · {} fps · currently temporary",
                    screen.settings.width, screen.settings.height, screen.settings.fps
                ),
                [el(kit::toggle(
                    ("draft-include", id as usize),
                    !self.draft.excluded.contains(&id),
                    format!("Include {}", screen.name),
                    move |include, _, cx| {
                        let _ = include_view.update(cx, |this, cx| {
                            if include {
                                this.draft.excluded.remove(&id);
                            } else {
                                this.draft.excluded.insert(id);
                            }
                            cx.notify();
                        });
                    },
                ))],
                cx,
            )));
        }
        for (index, (width, height)) in self.draft.extra.clone().into_iter().enumerate() {
            let remove_view = view.clone();
            screens.push(el(kit::item(
                Some(el(kit::monitor_icon(false, false, cx))),
                format!("New screen {}", index + 1),
                format!("{width}×{height} · created with the session"),
                [el(kit::tool_button(("draft-drop", index), "Remove").on_click(
                    move |_, _, cx| {
                        let _ = remove_view.update(cx, |this, cx| {
                            if index < this.draft.extra.len() {
                                this.draft.extra.remove(index);
                            }
                            cx.notify();
                        });
                    },
                ))],
                cx,
            )));
        }
        let choose_view = view.clone();
        let add_view = view.clone();
        screens.push(el(div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .py(px(10.))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("New screen"),
            )
            .child(kit::select(
                "draft-resolution",
                COMMON_RESOLUTIONS
                    .iter()
                    .enumerate()
                    .map(|(index, (w, h))| {
                        Choice::new(format!("{w} × {h}"), index == self.draft.resolution)
                    })
                    .collect(),
                move |index, _, cx| {
                    let _ = choose_view.update(cx, |this, cx| {
                        this.draft.resolution = index;
                        cx.notify();
                    });
                },
            ))
            .child(
                Button::new("draft-add-screen")
                    .icon(IconName::Plus)
                    .label("Add")
                    .outline()
                    .small()
                    .on_click(move |_, _, cx| {
                        let _ = add_view.update(cx, |this, cx| {
                            if this.draft.extra.len() < MAX_SCREENS {
                                this.draft
                                    .extra
                                    .push(COMMON_RESOLUTIONS[this.draft.resolution]);
                            }
                            cx.notify();
                        });
                    }),
            )));

        let cancel_state = self.state.clone();
        let mut page = vec![
            el(kit::hint(
                "A session is a shared login. Any viewer signed in to it on the same network connects automatically and gets the same screens and layout.",
                cx,
            )
            .text_size(px(13.))),
            el(kit::section(kit::section_label("Login", cx), login)),
            el(div()
                .flex()
                .flex_col()
                .gap_2()
                .child(kit::section_label("Starting screens", cx))
                .child(kit::card(screens, cx))
                .child(kit::hint(
                    "Optional: you can add or move screens into the session later.",
                    cx,
                ))),
        ];
        if let Some(error) = &self.draft.error {
            page.push(el(div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(error.clone())));
        }
        page.push(el(div()
            .flex()
            .gap_2()
            .child(
                Button::new("draft-cancel")
                    .label("Cancel")
                    .outline()
                    .on_click(move |_, window, _| {
                        with_state(&cancel_state, |state| state.page = Page::Home);
                        window.refresh();
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("draft-create")
                    .label("Create session")
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.create_session(window, cx))),
            )));
        page
    }

    fn create_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let username = self.fields.text("draft:name", cx).unwrap_or_default();
        let password =
            bridge_core::sessions::Secret(self.fields.text("draft:password", cx).unwrap_or_default());
        if let Err(error) = bridge_core::sessions::validate_login(username.trim(), &password.0) {
            self.draft.error = Some(error.to_string());
            cx.notify();
            return;
        }
        self.fields.set_text("draft:password", "", window, cx);
        let shared = self.state.clone();
        let excluded = self.draft.excluded.clone();
        let extra = self.draft.extra.clone();
        let auto_connect = self.draft.auto_connect;
        let username = username.trim().to_owned();
        // Password derivation is deliberately slow; keep it off the UI thread.
        std::thread::spawn(move || {
            let snapshot = shared.lock().ok().map(|state| {
                (
                    state.sessions.clone(),
                    state
                        .screens
                        .iter()
                        .filter(|screen| {
                            !screen.removing
                                && !excluded.contains(&screen.screen_id)
                                && state.sessions.owner(screen.screen_id).is_none()
                        })
                        .map(|screen| SavedScreen {
                            position: screen.display_position,
                            screen_id: screen.screen_id,
                            name: screen.name.clone(),
                            settings: screen.settings.clone(),
                        })
                        .collect::<Vec<_>>(),
                )
            });
            let Some((mut store, screens)) = snapshot else {
                return;
            };
            let result = store.create(username, &password.0, screens);
            let Ok(mut state) = shared.lock() else {
                return;
            };
            match result {
                Ok(session_id) => {
                    let Some(mut session) = store.sessions.pop() else {
                        return;
                    };
                    if state.sessions.sessions.iter().any(|old| {
                        old.contract.username == session.contract.username
                            || old.contract.screens.iter().any(|a| {
                                session
                                    .contract
                                    .screens
                                    .iter()
                                    .any(|b| a.screen_id == b.screen_id)
                            })
                    }) {
                        state.alert("Session creation conflicted with another change; retry");
                        return;
                    }
                    session.auto_connect = auto_connect;
                    state.sessions.sessions.push(session);
                    for (width, height) in extra {
                        let settings = VideoSettings {
                            width,
                            height,
                            ..Default::default()
                        };
                        add_host_screen(&mut state, Some(&session_id), settings);
                    }
                    state.expanded.insert(session_id);
                    state.page = Page::Home;
                    state.alerting = false;
                    state.message =
                        "Session created · sign in from the viewer with its name and password"
                            .into();
                    state.diagnostics.record("session", &state.message);
                }
                Err(error) => state.alert(format!("Session creation failed: {error}")),
            }
        });
        cx.notify();
    }

    // ───────────────────────── Screen ─────────────────────────

    fn codec_field(
        &mut self,
        id: ScreenId,
        codec: Codec,
        field: &'static CodecField,
        settings: &VideoSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shared = self.state.clone();
        let value = settings.codec_value(codec, field);
        let key = format!("screen:{id}:codec:{codec:?}:{}", field.key);
        let row = match field.control {
            Control::Segmented(choices) | Control::Select(choices) => {
                let options = choices
                    .iter()
                    .map(|choice| {
                        Choice::new(choice.label, value.as_choice() == Some(choice.value))
                            .unavailable(choice.unavailable.or(field.unavailable))
                    })
                    .collect::<Vec<_>>();
                let pick = move |index: usize, _: &mut Window, _: &mut App| {
                    edit(&shared, id, |settings| {
                        settings.set_codec_value(
                            codec,
                            field,
                            OptionValue::Choice(choices[index].value.into()),
                        )
                    })
                };
                if matches!(field.control, Control::Segmented(_)) {
                    kit::row(field.label, kit::segmented(key, options, pick, cx), cx)
                } else {
                    kit::row(
                        field.label,
                        kit::select(SharedString::from(key), options, pick),
                        cx,
                    )
                }
            }
            Control::Toggle { .. } => kit::row_wide(
                field.label,
                None,
                kit::toggle(
                    SharedString::from(key),
                    value.as_bool().unwrap_or(false),
                    field.label,
                    move |enabled, _, _| {
                        edit(&shared, id, |settings| {
                            settings.set_codec_value(codec, field, OptionValue::Bool(enabled))
                        })
                    },
                )
                .disabled(field.unavailable.is_some()),
                cx,
            ),
            Control::Number { min, max, unit, .. } => {
                let number = value.as_int().unwrap_or(min);
                let input = self.fields.input(
                    &key,
                    &number.to_string(),
                    InputSpec {
                        digits: Some(4),
                        ..Default::default()
                    },
                    window,
                    cx,
                    move |this: &mut Self, text, committed, _, _| {
                        if let (true, Ok(value)) = (committed, text.parse::<i64>()) {
                            edit(&this.state, id, |settings| {
                                settings.set_codec_value(
                                    codec,
                                    field,
                                    OptionValue::Int(value.clamp(min, max)),
                                )
                            });
                        }
                    },
                );
                let unit = if field.key == "keyframe_interval" {
                    format!(
                        "{unit} ({:.1} s @ {} fps)",
                        number as f32 / settings.fps.max(1) as f32,
                        settings.fps
                    )
                } else {
                    unit.to_owned()
                };
                kit::row(
                    field.label,
                    kit::inline()
                        .child(div().w(px(90.)).child(
                            Input::new(&input)
                                .small()
                                .disabled(field.unavailable.is_some()),
                        ))
                        .child(kit::unit(unit, cx)),
                    cx,
                )
            }
            Control::Range { min, max, .. } => {
                let number = value.as_int().unwrap_or(min);
                let slider = self.fields.slider(
                    &key,
                    number as f32,
                    min as f32,
                    max as f32,
                    1.,
                    window,
                    cx,
                    move |this: &mut Self, position, _, _| {
                        edit(&this.state, id, |settings| {
                            settings.set_codec_value(
                                codec,
                                field,
                                OptionValue::Int(position.round() as i64),
                            )
                        });
                    },
                );
                kit::row(
                    field.label,
                    kit::inline()
                        .child(div().flex_1().child(
                            Slider::new(&slider).disabled(field.unavailable.is_some()),
                        ))
                        .child(kit::unit(format!("{number} / {max}"), cx)),
                    cx,
                )
            }
        };
        match field.unavailable {
            Some(reason) => el(div()
                .id(SharedString::from(format!("off:{id}:{}", field.key)))
                .opacity(0.5)
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(reason).build(window, cx)
                })
                .child(row)),
            None => el(row),
        }
    }

    fn audio_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        // Local interface state only: every handler writes the draft and nothing else.
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
        let choices = |labels: &[&'static str], selected: usize| {
            labels
                .iter()
                .enumerate()
                .map(|(index, label)| Choice::new(*label, index == selected))
                .collect::<Vec<_>>()
        };
        let bitrate = self.fields.input(
            "audio:bitrate",
            &self.audio.bitrate.clone(),
            InputSpec {
                digits: Some(4),
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, text, _, _, _| this.audio.bitrate = text,
        );
        let offset = self.fields.input(
            "audio:offset",
            &self.audio.offset.clone(),
            InputSpec {
                digits: Some(4),
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, text, _, _, _| this.audio.offset = text,
        );
        let audio = &self.audio;
        let main = kit::card(
            [
                el(kit::row_wide(
                    "Stream audio",
                    None,
                    kit::toggle("audio-enabled", audio.enabled, "Stream audio", set!(enabled)),
                    cx,
                )),
                el(kit::row(
                    "Source",
                    kit::select(
                        "audio-source",
                        choices(
                            &[
                                "System audio (this screen’s apps)",
                                "Whole Mac",
                                "Input device…",
                            ],
                            audio.source,
                        ),
                        set!(source),
                    ),
                    cx,
                )),
                el(kit::row(
                    "Codec",
                    kit::segmented(
                        "audio-codec",
                        choices(&["Opus", "AAC", "PCM"], audio.codec),
                        set!(codec),
                        cx,
                    ),
                    cx,
                )),
                el(kit::row(
                    "Bitrate",
                    kit::inline()
                        .child(div().w(px(90.)).child(Input::new(&bitrate).small()))
                        .child(kit::unit("kbps", cx)),
                    cx,
                )),
                el(kit::row(
                    "Sample rate",
                    kit::segmented(
                        "audio-rate",
                        choices(&["44.1 kHz", "48 kHz"], audio.sample_rate),
                        set!(sample_rate),
                        cx,
                    ),
                    cx,
                )),
                el(kit::row(
                    "Channels",
                    kit::segmented(
                        "audio-channels",
                        choices(&["Mono", "Stereo", "5.1"], audio.channels),
                        set!(channels),
                        cx,
                    ),
                    cx,
                )),
                el(kit::row(
                    "A/V sync offset",
                    kit::inline()
                        .child(div().w(px(90.)).child(Input::new(&offset).small()))
                        .child(kit::unit("ms", cx)),
                    cx,
                )),
            ],
            cx,
        );
        let open_view = view.clone();
        let mut options = vec![el(kit::accordion_header(
            "audio-options",
            ["Opus options", "AAC options", "PCM options"][audio.codec.min(2)],
            audio.options_open,
            move |_, cx| {
                let _ = open_view.update(cx, |this, cx| {
                    this.audio.options_open = !this.audio.options_open;
                    cx.notify();
                });
            },
            cx,
        ))];
        if audio.options_open {
            // Same idea as video: the rows belong to the selected audio codec.
            match audio.codec {
                0 => options.extend([
                    el(kit::row(
                        "Application",
                        kit::segmented(
                            "opus-application",
                            choices(&["Low delay", "Audio", "VoIP"], audio.opus_application),
                            set!(opus_application),
                            cx,
                        ),
                        cx,
                    )),
                    el(kit::row(
                        "Frame size",
                        kit::select(
                            "opus-frame",
                            choices(&["10 ms", "20 ms"], audio.opus_frame),
                            set!(opus_frame),
                        ),
                        cx,
                    )),
                    el(kit::row_wide(
                        "Variable bitrate",
                        None,
                        kit::toggle("opus-vbr", audio.opus_vbr, "Variable bitrate", set!(opus_vbr)),
                        cx,
                    )),
                    el(kit::row_wide(
                        "Forward error correction",
                        None,
                        kit::toggle("opus-fec", audio.opus_fec, "FEC", set!(opus_fec)),
                        cx,
                    )),
                ]),
                1 => options.extend([
                    el(kit::row(
                        "Profile",
                        kit::segmented(
                            "aac-profile",
                            choices(&["LC", "HE"], audio.aac_profile),
                            set!(aac_profile),
                            cx,
                        ),
                        cx,
                    )),
                    el(kit::row(
                        "Bitrate mode",
                        kit::select(
                            "aac-mode",
                            choices(&["Constant", "Variable"], audio.aac_mode),
                            set!(aac_mode),
                        ),
                        cx,
                    )),
                ]),
                _ => options.push(el(kit::row(
                    "Bit depth",
                    kit::segmented(
                        "pcm-depth",
                        choices(&["16-bit", "24-bit"], audio.pcm_depth),
                        set!(pcm_depth),
                        cx,
                    ),
                    cx,
                ))),
            }
        }
        el(kit::dimmed(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(kit::section_header(
                    "Audio",
                    kit::pill("Not available yet", Pill::Neutral, cx),
                    cx,
                ))
                .child(main)
                .child(kit::card(options, cx)),
        ))
    }

    fn screen_page(
        &mut self,
        state: &State,
        screen: &HostScreen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let id = screen.screen_id;
        let shared = self.state.clone();
        let view = cx.entity().downgrade();
        let settings = screen.settings.clone();
        let owner = state.sessions.owner(id);
        let key = |name: &str| format!("screen:{id}:{name}");

        // Identity: PIN for temporary screens, session login for remembered ones.
        let restart_state = shared.clone();
        let remove_state = shared.clone();
        let serving = state.session_commands.is_some();
        let identity = kit::card(
            [el(div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(12.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(kit::hint(
                            if owner.is_some() {
                                "Session login"
                            } else {
                                "Screen PIN"
                            },
                            cx,
                        ))
                        .child(
                            div()
                                .text_size(px(20.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(match owner {
                                    Some(session) => session.contract.username.clone(),
                                    None => pin_text(&screen.pin),
                                }),
                        ),
                )
                .child(
                    Button::new("host-restart-screen")
                        .label("Restart stream")
                        .outline()
                        .disabled(!serving || screen.removing)
                        .on_click(move |_, window, _| {
                            with_state(&restart_state, |state| {
                                if let Some(settings) =
                                    state.screen(id).map(|screen| screen.settings.clone())
                                {
                                    state.send(HostUiCommand::ChangeSettings(id, settings));
                                }
                            });
                            window.refresh();
                        }),
                )
                .child(
                    Button::new("host-remove-screen")
                        .label("Remove")
                        .danger()
                        .outline()
                        .disabled(screen.removing)
                        .on_click(move |_, window, _| {
                            request_remove_screen(&remove_state, id);
                            window.refresh();
                        }),
                ))],
            cx,
        );

        // Display & stream.
        let name = self.fields.input(
            &key("name"),
            &screen.name,
            InputSpec {
                placeholder: "Screen name",
                ..Default::default()
            },
            window,
            cx,
            move |this: &mut Self, text, committed, _, _| {
                let text = text.trim().chars().take(48).collect::<String>();
                if committed && !text.is_empty() {
                    with_state(&this.state, |state| {
                        if let Some(screen) = state.screen_mut(id) {
                            screen.name = text;
                        }
                    });
                }
            },
        );
        let portrait = settings.portrait();
        let listed = COMMON_RESOLUTIONS
            .iter()
            .map(|(w, h)| if portrait { (*h, *w) } else { (*w, *h) })
            .collect::<Vec<_>>();
        let custom = self.custom_resolution.contains(&id)
            || !listed.contains(&(settings.width, settings.height));
        let mut resolutions = listed
            .iter()
            .map(|(w, h)| {
                Choice::new(
                    format!("{w} × {h} ({})", aspect_label(*w, *h)),
                    !custom && (*w, *h) == (settings.width, settings.height),
                )
            })
            .collect::<Vec<_>>();
        resolutions.push(Choice::new("Custom…", custom));
        let resolution_state = shared.clone();
        let resolution_view = view.clone();
        let resolution_list = listed.clone();
        let resolution = kit::select(
            SharedString::from(key("resolution")),
            resolutions,
            move |index, _, cx| {
                let choice = resolution_list.get(index).copied();
                let _ = resolution_view.update(cx, |this, cx| {
                    if choice.is_some() {
                        this.custom_resolution.remove(&id);
                    } else {
                        this.custom_resolution.insert(id);
                    }
                    cx.notify();
                });
                if let Some((width, height)) = choice {
                    edit(&resolution_state, id, |settings| {
                        settings.width = width;
                        settings.height = height;
                    });
                }
            },
        );
        let mut resolution_control = div().flex().flex_col().gap_2().child(resolution);
        if custom {
            let width = self.fields.input(
                &key("width"),
                &settings.width.to_string(),
                InputSpec {
                    digits: Some(4),
                    ..Default::default()
                },
                window,
                cx,
                move |this: &mut Self, text, committed, _, _| {
                    if let (true, Ok(value)) = (committed, text.parse::<u32>()) {
                        edit(&this.state, id, |s| s.width = value.clamp(320, 7680) & !1);
                    }
                },
            );
            let height = self.fields.input(
                &key("height"),
                &settings.height.to_string(),
                InputSpec {
                    digits: Some(4),
                    ..Default::default()
                },
                window,
                cx,
                move |this: &mut Self, text, committed, _, _| {
                    if let (true, Ok(value)) = (committed, text.parse::<u32>()) {
                        edit(&this.state, id, |s| s.height = value.clamp(240, 4320) & !1);
                    }
                },
            );
            resolution_control = resolution_control.child(
                kit::inline()
                    .child(div().w(px(84.)).child(Input::new(&width).small()))
                    .child(kit::unit("×", cx))
                    .child(div().w(px(84.)).child(Input::new(&height).small()))
                    .child(kit::unit("px", cx)),
            );
        }
        let orientation_state = shared.clone();
        let orientation = kit::segmented(
            key("orientation"),
            vec![
                Choice::new("0°", !portrait),
                Choice::new("90°", portrait),
                Choice::new("180°", false)
                    .unavailable(Some("Rotate the picture on the viewer instead")),
                Choice::new("270°", false)
                    .unavailable(Some("Rotate the picture on the viewer instead")),
            ],
            move |index, _, _| {
                if (index == 1) != portrait {
                    edit(&orientation_state, id, |settings| {
                        std::mem::swap(&mut settings.width, &mut settings.height)
                    });
                }
            },
            cx,
        );
        let mut rates = COMMON_FRAME_RATES.to_vec();
        if !rates.contains(&settings.fps) {
            rates.push(settings.fps);
            rates.sort_unstable();
        }
        let rate_state = shared.clone();
        let rate_values = rates.clone();
        let frame_rate = kit::segmented(
            key("fps"),
            rates
                .iter()
                .map(|fps| Choice::new(fps.to_string(), *fps == settings.fps))
                .collect(),
            move |index, _, _| {
                let fps = rate_values[index];
                edit(&rate_state, id, |settings| settings.fps = fps);
            },
            cx,
        );
        let bitrate = self.fields.input(
            &key("bitrate"),
            &settings.bitrate_kbps.to_string(),
            InputSpec {
                digits: Some(6),
                ..Default::default()
            },
            window,
            cx,
            move |this: &mut Self, text, committed, _, _| {
                if let (true, Ok(value)) = (committed, text.parse::<u32>()) {
                    edit(&this.state, id, |s| s.bitrate_kbps = value.clamp(250, 100_000));
                }
            },
        );
        let bitrate_slider = self.fields.slider(
            &key("bitrate-slider"),
            settings.bitrate_kbps as f32,
            1_000.,
            80_000.,
            500.,
            window,
            cx,
            move |this: &mut Self, position, _, _| {
                edit(&this.state, id, |s| s.bitrate_kbps = position.round() as u32);
            },
        );
        let vsync_state = shared.clone();
        let display = kit::card(
            [
                el(kit::row("Name", Input::new(&name).small(), cx)),
                el(kit::row_with_hint(
                    "Resolution",
                    resolution_control,
                    "Resizes the virtual display on this Mac. Changes made in System Settings sync back here.",
                    cx,
                )),
                el(kit::row("Orientation", orientation, cx)),
                el(kit::row("Frame rate", frame_rate, cx)),
                el(kit::row(
                    "Bitrate",
                    kit::inline()
                        .child(div().w(px(110.)).child(Input::new(&bitrate).small()))
                        .child(kit::unit("kbps", cx))
                        .child(div().flex_1().child(Slider::new(&bitrate_slider))),
                    cx,
                )),
                el(kit::row_wide(
                    "VSync",
                    Some("Capture aligned to display refresh.".into()),
                    kit::toggle("vsync", settings.vsync, "VSync", move |enabled, _, _| {
                        edit(&vsync_state, id, |settings| settings.vsync = enabled)
                    }),
                    cx,
                )),
            ],
            cx,
        );

        // Codec: handled codecs stay visible; unavailable ones explain themselves.
        let codec = settings.codec();
        let availability = codec_availability(
            &mac::available_codecs(),
            screen.viewer_codecs().as_deref(),
        );
        let codec_state = shared.clone();
        let codec_choice = kit::segmented(
            key("codec"),
            availability
                .iter()
                .map(|entry| {
                    Choice::new(entry.codec.label(), entry.codec == codec)
                        .unavailable(entry.unavailable)
                })
                .collect(),
            move |index, _, _| {
                edit(&codec_state, id, |settings| {
                    settings.preferred_codec = Some(CodecOption {
                        codec: Codec::ALL[index],
                        acceleration: settings.acceleration(),
                    })
                })
            },
            cx,
        );
        let encoder_state = shared.clone();
        let encoder = kit::select(
            SharedString::from(key("encoder")),
            vec![
                Choice::new(
                    "VideoToolbox (hardware)",
                    settings.acceleration() == Acceleration::Hardware,
                ),
                Choice::new(
                    "VideoToolbox (software)",
                    settings.acceleration() == Acceleration::Software,
                ),
            ],
            move |index, _, _| {
                edit(&encoder_state, id, |settings| {
                    settings.preferred_codec = Some(CodecOption {
                        codec: settings.codec(),
                        acceleration: if index == 0 {
                            Acceleration::Hardware
                        } else {
                            Acceleration::Software
                        },
                    })
                })
            },
        );
        let preset = settings.codec_preset();
        let preset_state = shared.clone();
        let presets = [CodecPreset::LowLatency, CodecPreset::Quality, CodecPreset::Custom];
        let preset_select = kit::select(
            SharedString::from(key("preset")),
            presets
                .iter()
                .map(|entry| {
                    Choice::new(entry.label(), *entry == preset).unavailable(
                        (*entry == CodecPreset::Custom && preset != CodecPreset::Custom)
                            .then_some("shown once an option below is changed"),
                    )
                })
                .collect(),
            move |index, _, _| {
                edit(&preset_state, id, |settings| {
                    settings.apply_codec_preset(presets[index])
                })
            },
        );
        let codec_card = kit::card(
            [
                el(kit::row("Codec", codec_choice, cx)),
                el(kit::row("Encoder", encoder, cx)),
                el(kit::row("Preset", preset_select, cx)),
            ],
            cx,
        );
        let toggle_view = view.clone();
        let mut codec_rows = vec![el(kit::accordion_header(
            "codec-options",
            codec.options_title(),
            self.codec_open,
            move |_, cx| {
                let _ = toggle_view.update(cx, |this, cx| {
                    this.codec_open = !this.codec_open;
                    cx.notify();
                });
            },
            cx,
        ))];
        if self.codec_open {
            // The rows come from the selected codec's own schema.
            for field in codec_schema(codec) {
                if field.visible(&settings, codec) {
                    codec_rows.push(self.codec_field(id, codec, field, &settings, window, cx));
                }
            }
        }

        // Connected viewers.
        let mut viewers = Vec::new();
        for peer in &screen.viewers {
            let peer_id = peer.id;
            let selected = screen.stats_peer == Some(peer_id);
            let stats_state = shared.clone();
            let drop_state = shared.clone();
            viewers.push(el(kit::item(
                None,
                peer.name.clone(),
                format!("{} · video only", peer.status),
                [
                    el(kit::tool_button(
                        ("peer-stats", peer_id as usize),
                        if selected { "Stats target ✓" } else { "Use for stats" },
                    )
                    .on_click(move |_, window, _| {
                        with_state(&stats_state, |state| {
                            if let Some(screen) = state.screen_mut(id) {
                                screen.stats_peer = (!selected).then_some(peer_id);
                                screen.stats_view.reset();
                            }
                        });
                        window.refresh();
                    })),
                    el(kit::tool_button(("disconnect-peer", peer_id as usize), "Disconnect")
                        .on_click(move |_, window, _| {
                            with_state(&drop_state, |state| {
                                state.send(HostUiCommand::DisconnectPeer(id, peer_id))
                            });
                            window.refresh();
                        })),
                ],
                cx,
            )));
        }
        if viewers.is_empty() {
            viewers.push(el(div()
                .px(px(14.))
                .py(px(10.))
                .child(kit::hint("No viewer connected · video only", cx))));
        }

        let mut page = vec![el(identity)];
        if screen.phase == Phase::Error {
            page.push(el(div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(screen.status.clone())));
        }
        page.push(el(kit::section(
            kit::section_label("Display & stream", cx),
            display,
        )));
        page.push(el(div()
            .flex()
            .flex_col()
            .gap_2()
            .child(kit::section_label("Codec", cx))
            .child(codec_card)
            .child(kit::hint(
                "Struck-through codecs: supported by AirBridge but unavailable here — hover shows why (host encoder / viewer decoder / stream path).",
                cx,
            ))
            .child(kit::card(codec_rows, cx))));
        page.push(self.audio_section(window, cx));
        page.push(el(kit::section(
            kit::section_label("Connected viewers", cx),
            kit::card(viewers, cx),
        )));
        page
    }
}

impl Render for HostView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let settings_state = self.state.clone();
        let gear = el(kit::icon_button("host-settings", IconName::Settings, "App settings")
            .on_click(move |_, _, cx| open_settings(settings_state.clone(), cx)));
        let alert = state.alerting.then(|| {
            let dismiss = self.state.clone();
            el(div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(16.))
                .py(px(8.))
                .bg(cx.theme().danger.opacity(0.1))
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(div().flex_1().min_w_0().child(state.message.clone()))
                .child(
                    kit::tool_button("host-dismiss-alert", "Dismiss").on_click(
                        move |_, window, _| {
                            with_state(&dismiss, |state| state.alerting = false);
                            window.refresh();
                        },
                    ),
                ))
        });
        let back = |shared: Arc<Mutex<State>>| {
            kit::icon_button("host-back", IconName::ChevronLeft, "Back to screens").on_click(
                move |_, window, _| {
                    with_state(&shared, |state| state.page = Page::Home);
                    window.refresh();
                },
            )
        };
        let (actions, crumb, page) = match state.page.clone() {
            Page::Screen(id) if state.screen(id).is_some() => {
                let screen = state.screen(id).unwrap().clone();
                let detail = match state.sessions.owner(id) {
                    Some(session) => format!(
                        "{} · {}",
                        session.contract.display_name(),
                        slot_label(&state, &screen)
                    ),
                    None => format!("Temporary · {}", slot_label(&state, &screen)),
                };
                let crumb = kit::crumb(
                    back(self.state.clone()),
                    screen.name.clone(),
                    detail,
                    Some(phase_pill(&screen, cx)),
                    cx,
                );
                let actions = vec![
                    el(Button::new("host-stats")
                        .icon(IconName::ChartPie)
                        .label("Stats")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.inspect_stats(id, cx)))),
                    el(Button::new("host-logs")
                        .icon(IconName::FileText)
                        .label("Logs")
                        .outline()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| this.open_logs(id, cx)))),
                    gear,
                ];
                let page = self.screen_page(&state, &screen, window, cx);
                (actions, Some(el(crumb)), page)
            }
            Page::NewSession => {
                let crumb = kit::crumb(back(self.state.clone()), "New session", "", None, cx);
                let page = self.new_session(&state, window, cx);
                (vec![gear], Some(el(crumb)), page)
            }
            _ => (vec![gear], None, self.home(&state, cx)),
        };
        let strip = (alert.is_some() || crumb.is_some())
            .then(|| el(div().flex().flex_col().children(alert).children(crumb)));
        bridge_ui::shell("AirBridge · Host", actions, strip, stack(page), cx)
    }
}

// ───────────────────────── App settings window ─────────────────────────

struct SettingsView {
    state: Arc<Mutex<State>>,
    fields: Fields,
    session: Option<String>,
    confirm_sign_out: bool,
    capture_granted: bool,
    notice: Option<String>,
}

fn change_password(shared: Arc<Mutex<State>>, session_id: String, password: String) {
    let password = bridge_core::sessions::Secret(password);
    std::thread::spawn(move || {
        let salt = shared
            .lock()
            .ok()
            .and_then(|state| state.sessions.session(&session_id).map(|s| s.salt));
        let Some(salt) = salt else { return };
        let key = bridge_core::sessions::password_key(&password.0, &salt);
        if let Ok(mut state) = shared.lock() {
            match key {
                Ok(key) => {
                    if let Some(session) = state.sessions.session_mut(&session_id) {
                        session.key = key;
                        state.message =
                            "Session password changed · viewers sign in again with the new one"
                                .into();
                        state.diagnostics.record("session", &state.message);
                    }
                }
                Err(error) => state.alert(format!("Password change failed: {error}")),
            }
        }
    });
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let view = cx.entity().downgrade();
        let shared = self.state.clone();
        if self
            .session
            .as_ref()
            .is_none_or(|id| state.sessions.session(id).is_none())
        {
            self.session = state
                .sessions
                .sessions
                .first()
                .map(|s| s.contract.session_id.clone());
            self.confirm_sign_out = false;
        }

        let permission_view = view.clone();
        let permissions = kit::card(
            [el(kit::row_wide(
                "Screen Recording",
                Some(
                    if self.capture_granted {
                        "Granted · required to capture virtual screens"
                    } else {
                        "Not granted · required to capture virtual screens. Grant access, then restart AirBridge."
                    }
                    .into(),
                ),
                kit::tool_button("settings-open-system", "Open System Settings").on_click(
                    move |_, _, cx| {
                        let granted = mac::request_capture_access();
                        let _ = std::process::Command::new("open")
                            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
                            .spawn();
                        let _ = permission_view.update(cx, |this, cx| {
                            this.capture_granted = granted;
                            cx.notify();
                        });
                    },
                ),
                cx,
            ))],
            cx,
        );

        let mut account = Vec::new();
        if let Some(session) = self
            .session
            .clone()
            .and_then(|id| state.sessions.session(&id).cloned())
        {
            let session_id = session.contract.session_id.clone();
            if state.sessions.sessions.len() > 1 {
                let ids = state
                    .sessions
                    .sessions
                    .iter()
                    .map(|s| s.contract.session_id.clone())
                    .collect::<Vec<_>>();
                let pick_view = view.clone();
                account.push(el(kit::row(
                    "Session",
                    kit::select(
                        "settings-session",
                        state
                            .sessions
                            .sessions
                            .iter()
                            .map(|s| {
                                Choice::new(
                                    s.contract.display_name().to_owned(),
                                    s.contract.session_id == session_id,
                                )
                            })
                            .collect(),
                        move |index, _, cx| {
                            let id = ids.get(index).cloned();
                            let _ = pick_view.update(cx, |this, cx| {
                                this.session = id;
                                this.confirm_sign_out = false;
                                cx.notify();
                            });
                        },
                    ),
                    cx,
                )));
            }
            let name_id = session_id.clone();
            let name = self.fields.input(
                &format!("settings:name:{session_id}"),
                session.contract.display_name(),
                InputSpec::default(),
                window,
                cx,
                move |this: &mut Self, text, committed, _, _| {
                    let text = text.trim().chars().take(64).collect::<String>();
                    if committed && !text.is_empty() {
                        with_state(&this.state, |state| {
                            if let Some(session) = state.sessions.session_mut(&name_id) {
                                if session.contract.display_name() != text {
                                    session.contract.label = text;
                                    session.contract.revision += 1;
                                }
                            }
                        });
                    }
                },
            );
            let password_id = session_id.clone();
            let password = self.fields.input(
                &format!("settings:password:{session_id}"),
                "",
                InputSpec {
                    placeholder: "New password, then Enter",
                    masked: true,
                    ..Default::default()
                },
                window,
                cx,
                move |this: &mut Self, text, committed, _, _| {
                    // Enter commits; leaving the field with text in it does not.
                    let _ = (text, committed, &password_id, &this.state);
                },
            );
            let apply_view = view.clone();
            let apply_id = session_id.clone();
            let password_key = format!("settings:password:{session_id}");
            account.push(el(kit::row_with_hint(
                "Account",
                Input::new(&name).small(),
                format!("Viewers sign in as “{}”.", session.contract.username),
                cx,
            )));
            account.push(el(kit::row(
                "Session password",
                kit::inline()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&password).small().mask_toggle()),
                    )
                    .child(kit::tool_button("settings-password-apply", "Change").on_click(
                        cx.listener(move |this, _, window, cx| {
                            let text = this.fields.text(&password_key, cx).unwrap_or_default();
                            if text.len() > 256 {
                                this.notice = Some("Password must be at most 256 bytes".into());
                            } else {
                                change_password(this.state.clone(), apply_id.clone(), text);
                                this.fields.set_text(&password_key, "", window, cx);
                                this.notice = Some("Password changed".into());
                            }
                            let _ = &apply_view;
                            cx.notify();
                        }),
                    )),
                cx,
            )));
            let auto_state = shared.clone();
            let auto_id = session_id.clone();
            account.push(el(kit::row_wide(
                "Auto-connect on this device",
                None,
                kit::toggle(
                    "settings-auto",
                    session.auto_connect,
                    "Auto-connect",
                    move |enabled, _, _| {
                        with_state(&auto_state, |state| {
                            if let Some(session) = state.sessions.session_mut(&auto_id) {
                                session.auto_connect = enabled;
                            }
                        })
                    },
                ),
                cx,
            )));
            let awake_state = shared.clone();
            let awake_id = session_id.clone();
            account.push(el(kit::row_wide(
                "Keep this Mac awake while streaming",
                (!state.power_notice.is_empty()).then(|| state.power_notice.clone().into()),
                kit::toggle(
                    "settings-keep-awake",
                    session.contract.keep_awake,
                    "Keep this Mac awake",
                    move |enabled, _, _| {
                        with_state(&awake_state, |state| {
                            if let Some(session) = state.sessions.session_mut(&awake_id) {
                                session.contract.keep_awake = enabled;
                                session.contract.revision += 1;
                            }
                        })
                    },
                ),
                cx,
            )));
            let policy_state = shared.clone();
            let policy_id = session_id.clone();
            account.push(el(kit::row(
                "Viewer lock policy",
                kit::select(
                    "settings-lock-policy",
                    ViewerLockPolicy::ALL
                        .iter()
                        .map(|policy| {
                            Choice::new(
                                policy.label(),
                                *policy == session.contract.viewer_lock_policy,
                            )
                        })
                        .collect(),
                    move |index, _, _| {
                        with_state(&policy_state, |state| {
                            if let Some(session) = state.sessions.session_mut(&policy_id) {
                                session.contract.viewer_lock_policy = ViewerLockPolicy::ALL[index];
                                session.contract.revision += 1;
                            }
                        })
                    },
                ),
                cx,
            )));
            let confirm = self.confirm_sign_out;
            let out_view = view.clone();
            let out_state = shared.clone();
            let out_id = session_id.clone();
            account.push(el(kit::row_wide(
                "Sign out",
                Some(
                    if confirm {
                        "Removes this session, its screens and saved viewers from this Mac."
                    } else {
                        "Removes the session from this Mac."
                    }
                    .into(),
                ),
                Button::new("settings-sign-out")
                    .label(if confirm { "Confirm sign out" } else { "Sign out" })
                    .danger()
                    .outline()
                    .small()
                    .on_click(move |_, _, cx| {
                        if confirm {
                            delete_session(&out_state, &out_id);
                        }
                        let _ = out_view.update(cx, |this, cx| {
                            this.confirm_sign_out = !confirm;
                            cx.notify();
                        });
                    }),
                cx,
            )));
        } else {
            account.push(el(div().px(px(14.)).py(px(12.)).child(kit::hint(
                "No session yet. Create one from “New session” in the main window.",
                cx,
            ))));
        }

        let port = self.fields.input(
            "settings:port",
            &state.settings.listen_port.to_string(),
            InputSpec {
                digits: Some(5),
                ..Default::default()
            },
            window,
            cx,
            |this: &mut Self, text, committed, _, _| {
                if !committed {
                    return;
                }
                match text.parse::<u16>() {
                    Ok(port) if port >= 1024 => with_state(&this.state, |state| {
                        if state.settings.listen_port != port {
                            state.settings.listen_port = port;
                            if let Err(error) = state.settings.save() {
                                state.alert(format!("Settings save failed: {error}"));
                            }
                        }
                    }),
                    _ => this.notice = Some("Listen port must be between 1024 and 65535".into()),
                }
            },
        );
        let advertise_state = shared.clone();
        let network = kit::card(
            [
                el(kit::row_with_hint(
                    "Listen port",
                    Input::new(&port).small(),
                    "Applies the next time AirBridge starts.",
                    cx,
                )),
                el(kit::row_wide(
                    "Advertise on local network (mDNS)",
                    None,
                    kit::toggle(
                        "settings-advertise",
                        state.settings.advertise,
                        "Advertise via mDNS",
                        move |enabled, _, _| {
                            with_state(&advertise_state, |state| {
                                state.settings.advertise = enabled;
                                if let Err(error) = state.settings.save() {
                                    state.alert(format!("Settings save failed: {error}"));
                                }
                            })
                        },
                    ),
                    cx,
                )),
            ],
            cx,
        );

        let mut page = vec![
            el(kit::section(kit::section_label("Permissions", cx), permissions)),
            el(kit::section(
                kit::section_label("Account & session", cx),
                kit::card(account, cx),
            )),
            el(kit::section(kit::section_label("Network", cx), network)),
        ];
        if let Some(notice) = &self.notice {
            page.push(el(kit::hint(notice.clone(), cx)));
        }
        bridge_ui::shell("AirBridge · Settings", vec![], None, stack(page), cx)
    }
}

fn open_settings(state: Arc<Mutex<State>>, cx: &mut App) {
    let shared = state.clone();
    let result = bridge_ui::open_auxiliary(
        "host-settings".into(),
        size(px(480.), px(620.)),
        cx,
        move |window, cx| {
            window.set_window_title("AirBridge · Settings");
            let view = cx.new(|_| SettingsView {
                state,
                fields: Fields::default(),
                session: None,
                confirm_sign_out: false,
                capture_granted: mac::capture_allowed(),
                notice: None,
            });
            let weak = view.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    if weak
                        .update(cx, |this, cx| {
                            this.capture_granted = mac::capture_allowed();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            view
        },
    );
    if let Err(error) = result {
        set_alert(&shared, format!("Cannot open settings: {error:#}"));
    }
}
