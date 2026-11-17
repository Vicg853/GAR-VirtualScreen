#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::discovery::{DiscoveredScreen, DiscoveredSession};

    fn state() -> Arc<Mutex<State>> {
        Arc::new(Mutex::new(initial_state()))
    }
    fn screen(id: ScreenId, generation: u64) -> ClientScreen {
        let frame = Arc::new(Mutex::new(VideoFrame::default()));
        ClientScreen {
            config: ScreenConfig {
                screen_id: id,
                name: format!("Screen {id}"),
                slot: "Slot A".into(),
                revision: 2,
                generation,
                settings: Default::default(),
                instrumented: false,
                codec: CodecOption {
                    codec: Codec::H264,
                    acceleration: Acceleration::Software,
                },
            },
            status: "Streaming".into(),
            terminal: false,
            cleanup_reported: false,
            removing: false,
            cancel: watch::channel(false).0,
            video_window: VideoWindow::unavailable("Test backend".into(), frame.clone()),
            frame,
            diagnostics: Arc::new(Diagnostics::new("viewer-lifecycle-test")),
            bytes: 0,
            received_frames: 0,
            host_frames_sent: 0,
            host_frames_dropped: 0,
            bitrate_kbps: 0,
            telemetry: Telemetry::new(),
            stats_view: StatsView::default(),
        }
    }
    fn contract(screens: &[ScreenId]) -> bridge_core::sessions::SessionContract {
        bridge_core::sessions::SessionContract {
            version: bridge_core::sessions::STORE_VERSION,
            session_id: bridge_core::sessions::new_id().unwrap(),
            host_id: bridge_core::sessions::new_id().unwrap(),
            username: "alice".into(),
            label: String::new(),
            revision: 1,
            keep_awake: false,
            viewer_lock_policy: Default::default(),
            screens: screens
                .iter()
                .map(|id| bridge_core::sessions::SavedScreen {
                    position: None,
                    screen_id: *id,
                    name: format!("Screen {id}"),
                    settings: Default::default(),
                })
                .collect(),
        }
    }
    fn remembered_state() -> State {
        let mut s = initial_state();
        let contract = contract(&[]);
        s.username = "alice".into();
        s.remember = true;
        s.auto_connect = true;
        s.contract = Some(contract.clone());
        s.remembered = Some(bridge_core::sessions::SavedViewerSession {
            contract,
            viewer_id: s.stable_viewer.clone(),
            key: bridge_core::sessions::Secret("11".repeat(32)),
            preferences: Default::default(),
            auto_connect: true,
        });
        s
    }
    fn host(name: &str, host_id: &str, address: &str) -> DiscoveredHost {
        DiscoveredHost {
            name: name.into(),
            label: name.into(),
            addresses: vec![address.parse().unwrap()],
            host_id: host_id.into(),
            screens: vec![],
            sessions: vec![],
        }
    }
    fn entry(id: u64, state: Arc<Mutex<State>>) -> (LocalScreen, mpsc::Receiver<Command>) {
        let (commands, receiver) = mpsc::channel(16);
        (
            LocalScreen {
                id,
                state,
                commands,
                shutdown: watch::channel(false).0,
                thread: Arc::new(Mutex::new(None)),
            },
            receiver,
        )
    }

    #[test]
    fn late_diagnostic_batches_cannot_update_replacement_generation() {
        let shared = state();
        let mut s = screen(1, 7);
        s.config.instrumented = true;
        let telemetry = s.telemetry.clone();
        shared.lock().unwrap().screens.push(s);
        ingest_diagnostic_batch(
            &shared,
            bridge_core::telemetry::DiagnosticBatch {
                screen_id: 1,
                generation: 6,
                events: vec![],
                stats: None,
                lost_events: 100,
            },
        );
        assert_eq!(telemetry.snapshot().peer_lost_events, 0);
        ingest_diagnostic_batch(
            &shared,
            bridge_core::telemetry::DiagnosticBatch {
                screen_id: 1,
                generation: 7,
                events: vec![],
                stats: None,
                lost_events: 3,
            },
        );
        assert_eq!(telemetry.snapshot().peer_lost_events, 3);
    }

    #[test]
    fn old_generation_events_cannot_close_or_change_replacement() {
        let shared = state();
        shared.lock().unwrap().screens.push(screen(1, 5));
        end_client_lane(&shared, 1, 4, "Old lane closed".into());
        update_client_screen_status(&shared, 1, 4, "Old lane ready".into());
        assert!(!shared.lock().unwrap().screens[0].terminal);
        end_client_lane(&shared, 1, 5, "Capture failed".into());
        update_client_screen_status(&shared, 1, 5, "Late ready".into());
        let state = shared.lock().unwrap();
        assert!(state.screens[0].terminal);
        assert!(*state.screens[0].cancel.borrow());
        assert_eq!(state.screens[0].status, "Capture failed");
    }

    #[test]
    fn host_viewer_lock_policy_overrides_without_changing_local_preference() {
        use bridge_core::sessions::ViewerLockPolicy;
        let mut state = remembered_state();
        state.preferences.keep_awake = false;
        state.contract.as_mut().unwrap().viewer_lock_policy = ViewerLockPolicy::KeepAwake;
        assert!(state.effective_keep_awake());
        assert!(!state.preferences.keep_awake);
        state.preferences.keep_awake = true;
        state.contract.as_mut().unwrap().viewer_lock_policy = ViewerLockPolicy::AllowIdle;
        assert!(!state.effective_keep_awake());
        assert!(state.preferences.keep_awake);
        state.contract.as_mut().unwrap().viewer_lock_policy = ViewerLockPolicy::ViewerChoice;
        assert!(state.effective_keep_awake());
    }

    #[test]
    fn auto_connection_needs_both_sides_and_the_session_on_the_network() {
        let mut s = remembered_state();
        let saved = s.contract.clone().unwrap();
        s.hosts.push(host("unrelated", "different-host", "127.0.0.1:4444"));
        assert!(s.auto_endpoint().is_none());
        // The right host, but it does not offer the session (inactive there).
        let mut known = host("known-host", &saved.host_id, "127.0.0.1:44990");
        s.hosts.push(known.clone());
        assert!(s.auto_endpoint().is_none());
        // Offered, with the host's side of auto-connect paused.
        known.sessions.push(DiscoveredSession {
            username: "alice".into(),
            name: "alice".into(),
            screens: 2,
            auto_connect: false,
            id_prefix: saved.session_id[..8].into(),
        });
        s.discovery_event(DiscoveryEvent::Found(known.clone()));
        assert_eq!(s.hosts.len(), 2, "a record replaces the host's previous one");
        assert!(s.auto_endpoint().is_none());
        known.sessions[0].auto_connect = true;
        s.discovery_event(DiscoveryEvent::Found(known.clone()));
        assert_eq!(s.auto_endpoint(), Some(known.addresses[0]));
        // This device's side pauses independently.
        s.auto_connect = false;
        assert!(s.auto_endpoint().is_none());
        s.auto_connect = true;
        s.auto_suspended = true;
        assert!(s.auto_endpoint().is_none());
        s.auto_suspended = false;
        s.remember = false;
        assert!(s.auto_endpoint().is_none());
    }

    #[test]
    fn discovery_replaces_slot_lists_wholesale_and_forgets_lost_hosts() {
        let shared = state();
        let mut state = shared.lock().unwrap();
        let mut record = host("mac", "host-id", "127.0.0.1:44990");
        record.screens = vec![
            DiscoveredScreen {
                screen_id: 1,
                name: "Temp 1".into(),
            },
            DiscoveredScreen {
                screen_id: 2,
                name: "Temp 2".into(),
            },
        ];
        state.discovery_event(DiscoveryEvent::Found(record.clone()));
        assert_eq!(state.hosts[0].screens.len(), 2);
        // The host removed a screen: its next record no longer lists it.
        record.screens.truncate(1);
        state.discovery_event(DiscoveryEvent::Found(record));
        assert_eq!(state.hosts.len(), 1);
        assert_eq!(state.hosts[0].screens.len(), 1, "no ghost of the removed screen");
        state.discovery_event(DiscoveryEvent::Lost("someone-else".into()));
        assert_eq!(state.hosts.len(), 1);
        state.discovery_event(DiscoveryEvent::Lost("mac".into()));
        assert!(state.hosts.is_empty());
    }

    #[tokio::test]
    async fn completed_media_task_is_not_polled_again_during_cleanup() {
        let mut task = Some(tokio::spawn(async {}));
        tokio::select! {
            result = async { task.as_mut().unwrap().await } => {
                assert!(result.is_ok());
                task.take();
            }
        }
        stop_lane_manager(task).await;
    }

    #[tokio::test]
    async fn panicked_media_task_is_not_polled_again_during_cleanup() {
        let mut task = Some(tokio::spawn(async {
            panic!("fixture media task failure");
        }));
        assert!(task.as_mut().unwrap().await.unwrap_err().is_panic());
        task.take();
        stop_lane_manager(task).await;
    }

    #[tokio::test]
    async fn active_media_task_is_cancelled_and_joined_during_cleanup() {
        let (ready, began) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = ready.send(());
            std::future::pending::<()>().await;
        });
        began.await.unwrap();
        stop_lane_manager(Some(task)).await;
    }

    #[tokio::test]
    async fn media_panic_is_scoped_and_unregisters_the_lane() {
        let key = (2, 7, MediaLane::Video);
        let active = Arc::new(Mutex::new(HashSet::from([key])));
        let (send, mut receive) = mpsc::channel(64);
        let registered = active.clone();
        let task = tokio::spawn(async move {
            let _registration = LaneRegistration {
                key,
                active: registered,
                events: send,
                shared: None,
            };
            panic!("simulated receiver failure");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(active.lock().unwrap().is_empty());
        assert!(matches!(
            receive.recv().await,
            Some(LaneNotice::Failed(2, 7, MediaLane::Video, _))
        ));
    }

    #[tokio::test]
    async fn a_late_settings_ack_cannot_recreate_a_removed_screen() {
        let shared = state();
        let screen = screen(1, 3);
        let mut ack = screen.config.clone();
        shared.lock().unwrap().screens.push(screen);
        remove_client_screen(&shared, 1).await;
        ack.generation = 4;
        install_client_screen(&shared, ack).await.unwrap();
        assert!(shared.lock().unwrap().screens.is_empty());
        clear_client_session(&shared);
        assert!(shared.lock().unwrap().removed_screens.is_empty());
    }

    #[test]
    fn capture_error_after_media_fin_keeps_the_actual_failure_visible() {
        let shared = state();
        shared.lock().unwrap().screens.push(screen(1, 3));
        end_client_lane(&shared, 1, 3, "Video lane closed".into());
        end_client_lane(&shared, 1, 3, "Capture failed: permission denied".into());
        let diagnostics = shared.lock().unwrap().screens[0].diagnostics.clone();
        let count = diagnostics.recent().len();
        end_client_lane(&shared, 1, 3, "Capture failed: permission denied".into());
        end_client_lane(&shared, 1, 3, "Video lane closed".into());
        assert_eq!(diagnostics.recent().len(), count);
        assert_eq!(
            shared.lock().unwrap().screens[0].status,
            "Capture failed: permission denied"
        );
    }

    #[tokio::test]
    async fn leaving_a_pin_screen_is_idempotent_and_ends_the_connection() {
        let shared = state();
        {
            let mut state = shared.lock().unwrap();
            state.connected = true;
            state.screens.push(screen(1, 5));
        }
        let (mut write, mut read) = tokio::io::duplex(4096);
        request_remove_screen(&shared, &mut write, 1).await.unwrap();
        request_remove_screen(&shared, &mut write, 1).await.unwrap();
        let request: ClientMessage = receive_json(&mut read).await.unwrap();
        assert!(matches!(request, ClientMessage::Stop));
        {
            let state = shared.lock().unwrap();
            assert!(state.screens[0].removing);
            assert!(*state.screens[0].cancel.borrow());
        }
        remove_client_screen(&shared, 1).await;
        let state = shared.lock().unwrap();
        assert!(state.connected);
        assert!(state.screens.is_empty());
    }

    #[tokio::test]
    async fn pausing_a_session_screen_is_remembered_and_only_detaches_that_screen() {
        let shared = state();
        {
            let mut state = shared.lock().unwrap();
            state.connected = true;
            state.contract = Some(contract(&[1, 2]));
            state.screens.push(screen(1, 5));
            state.screens.push(screen(2, 5));
        }
        let (mut write, mut read) = tokio::io::duplex(4096);
        request_remove_screen(&shared, &mut write, 1).await.unwrap();
        let request: ClientMessage = receive_json(&mut read).await.unwrap();
        assert!(matches!(
            request,
            ClientMessage::RemoveScreen { screen_id: 1, .. }
        ));
        remove_client_screen(&shared, 1).await;
        {
            let state = shared.lock().unwrap();
            assert!(!state.preferences.visible(1), "the pause is a saved preference");
            assert!(state.preferences.visible(2));
            assert_eq!(state.screens.len(), 1);
            assert!(!state.screens[0].removing);
        }
        // A paused slot is not resurrected by a late host message.
        let mut late = screen(1, 9).config;
        late.generation = 9;
        install_client_screen(&shared, late).await.unwrap();
        assert_eq!(shared.lock().unwrap().screens.len(), 1);
        // Pausing a slot whose host screen is not live still tells the host once.
        shared.lock().unwrap().contract = Some(contract(&[1, 2, 3]));
        request_remove_screen(&shared, &mut write, 3).await.unwrap();
        request_remove_screen(&shared, &mut write, 3).await.unwrap();
        let request: ClientMessage = receive_json(&mut read).await.unwrap();
        assert!(matches!(
            request,
            ClientMessage::RemoveScreen { screen_id: 3, .. }
        ));
        drop(write);
        assert!(receive_json::<_, ClientMessage>(&mut read).await.is_err());
        assert!(!shared.lock().unwrap().preferences.visible(3));
    }

    #[tokio::test]
    async fn user_closed_display_is_not_resurrected_by_settings_ack() {
        let shared = state();
        let screen = screen(1, 3);
        let mut ack = screen.config.clone();
        screen.frame.lock().unwrap().user_closed = true;
        ack.generation = 4;
        ack.revision = 3;
        shared.lock().unwrap().screens.push(screen);
        install_client_screen(&shared, ack).await.unwrap();
        assert_eq!(shared.lock().unwrap().screens[0].config.generation, 3);
    }

    #[test]
    fn disconnect_cancels_all_screens_and_clears_the_connection() {
        let shared = state();
        let first = screen(1, 2);
        let second = screen(2, 7);
        let cancellations = [first.cancel.clone(), second.cancel.clone()];
        {
            let mut state = shared.lock().unwrap();
            state.connected = true;
            state.connecting = true;
            state.screens = vec![first, second];
        }
        clear_client_session(&shared);
        clear_client_session(&shared);
        assert!(cancellations.iter().all(|cancel| *cancel.borrow()));
        let state = shared.lock().unwrap();
        assert!(state.screens.is_empty());
        assert!(!state.connected && !state.connecting);
    }

    #[test]
    fn app_shutdown_resets_network_state_without_opening_windows() {
        let shared = state();
        let screen = screen(1, 2);
        let cancel = screen.cancel.clone();
        {
            let mut state = shared.lock().unwrap();
            state.connected = true;
            state.screens.push(screen);
        }
        let (_commands, receive) = mpsc::channel(64);
        let (stop, stopped) = watch::channel(false);
        stop.send_replace(true);
        run_network(shared.clone(), receive, stopped).unwrap();
        assert!(*cancel.borrow());
        assert!(!shared.lock().unwrap().connected);
    }

    #[tokio::test]
    async fn connection_can_be_cancelled_during_startup() {
        let shared = state();
        let (send, mut receive) = mpsc::channel(64);
        let (_shutdown, shutdown) = watch::channel(false);
        send.try_send(Command::Disconnect).unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            connect(
                "127.0.0.1:44991".parse().unwrap(),
                "123456".into(),
                false,
                shared.clone(),
                &mut receive,
                shutdown,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(shared.lock().unwrap().screens.is_empty());
        assert!(shared.lock().unwrap().message.contains("cancelled"));
    }

    fn open(
        cx: &mut gpui_kit::TestAppContext,
        workspace: &Arc<Mutex<Workspace>>,
        catalog: &Arc<Mutex<State>>,
    ) -> (gpui_kit::AnyWindowHandle, gpui_kit::Entity<ViewerView>) {
        cx.update(gpui_kit::init);
        let (workspace, catalog) = (workspace.clone(), catalog.clone());
        cx.update(|cx| {
            let (handle, view) = gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| ViewerView::new(workspace, catalog))
            })
            .unwrap();
            (handle.into(), view)
        })
    }

    #[gpui_kit::test]
    fn first_launch_starts_onboarding_and_saved_sessions_start_on_the_list(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt;
        let catalog = state();
        let empty = Arc::new(Mutex::new(Workspace::default()));
        let (handle, view) = open(cx, &empty, &catalog);
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("join-continue").is_some());
                assert!(window.try_find("add-host-session").is_none());
                assert!(window.try_find("viewer-settings").is_none());
                // Nothing selected and nothing typed: stay on the page with a reason.
                window.click("join-continue", cx);
            })
            .unwrap();
        cx.read(|cx| {
            assert_eq!(view.read(cx).page, Page::Join);
            assert!(view.read(cx).join.error.is_some());
        });
        assert!(empty.lock().unwrap().entries.is_empty());

        let (saved, _commands) = entry(1, Arc::new(Mutex::new(remembered_state())));
        let workspace = Arc::new(Mutex::new(Workspace {
            entries: vec![saved],
            ..Default::default()
        }));
        let (handle, view) = open(cx, &workspace, &catalog);
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("add-host-session").is_some());
                assert!(window.try_find("viewer-stats").is_none());
                window.click(("local-session", 1_usize), cx);
            })
            .unwrap();
        cx.read(|cx| assert_eq!(view.read(cx).page, Page::Session(1)));
    }

    #[gpui_kit::test]
    fn screen_view_changes_presentation_only_and_owns_stats_and_logs(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use gpui_kit::test::TestWindowExt;
        let mut session = remembered_state();
        session.contract = Some(contract(&[1, 2]));
        session.connected = true;
        session.screens.push(screen(1, 3));
        let shared = Arc::new(Mutex::new(session));
        let (local, mut commands) = entry(1, shared.clone());
        let workspace = Arc::new(Mutex::new(Workspace {
            entries: vec![local],
            ..Default::default()
        }));
        let (handle, view) = open(cx, &workspace, &state());
        cx.update(|cx| {
            view.update(cx, |this, cx| {
                this.refresh(cx);
                this.go(Page::Session(1), cx);
            })
        });
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("viewer-stats").is_none());
                // Pause the second screen, which is not streaming right now.
                window.click(("show-screen", 2_usize), cx);
                window.click(("configure-screen", 1_usize), cx);
            })
            .unwrap();
        assert!(!shared.lock().unwrap().preferences.visible(2));
        assert!(matches!(commands.try_recv(), Ok(Command::RemoveScreen(2))));
        cx.read(|cx| assert_eq!(view.read(cx).page, Page::Screen(1, 1)));
        handle
            .update(cx, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("viewer-stats").is_some());
                assert!(window.try_find("viewer-logs").is_some());
                // "Fill" is the second scaling segment.
                window.click(gpui_kit::ElementId::NamedInteger("display-scaling".into(), 1), cx);
                window.click("display-vsync", cx);
            })
            .unwrap();
        {
            let state = shared.lock().unwrap();
            let display = state.display(1);
            assert_eq!(display.scaling, Scaling::Fill);
            assert!(display.vsync);
            assert_eq!(
                state.screens[0].config.settings,
                Default::default(),
                "the incoming stream settings are untouched"
            );
        }
        // Only local presentation commands were issued; nothing addressed to the host.
        let mut issued = Vec::new();
        while let Ok(command) = commands.try_recv() {
            issued.push(command);
        }
        assert_eq!(issued.len(), 2);
        assert!(issued.iter().all(|command| matches!(command, Command::Present(1))));
        // The stream ending returns to the session instead of showing a stale screen.
        shared.lock().unwrap().screens.clear();
        cx.update(|cx| view.update(cx, |this, cx| this.refresh(cx)));
        cx.read(|cx| assert_eq!(view.read(cx).page, Page::Session(1)));
    }
}
