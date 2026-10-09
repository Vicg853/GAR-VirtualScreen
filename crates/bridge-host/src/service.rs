// Screen-owned capture and subscriber-owned transport. Included inside app.
mod service {
    use super::*;
    use bridge_core::config::{Acceleration, Codec as BridgeCodec};
    use bridge_core::{
        failure::{Failure, FailureCode, FailureScope},
        security::{authenticate_host_target, AuthenticatedTarget},
    };
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;
    use std::{collections::HashMap, sync::mpsc as native_mpsc};
    use tokio::task::JoinSet;

    const PEER_QUEUE: usize = 2;
    #[derive(Clone)]
    struct ScreenHandle {
        tx: mpsc::Sender<ScreenCommand>,
    }
    enum ScreenCommand {
        Join {
            id: u64,
            connection: quinn::Connection,
            codecs: Vec<CodecOption>,
            controls: mpsc::Sender<HostMessage>,
            reply: oneshot::Sender<Result<ScreenConfig>>,
        },
        Leave(u64),
        Settings {
            peer: Option<u64>,
            request: Option<RequestId>,
            settings: VideoSettings,
            revision: Option<u64>,
        },
        Diagnostics {
            peer: Option<u64>,
            enabled: bool,
        },
        Burst(Option<u64>),
        Disconnect(u64),
        Remove,
        VideoStopped {
            id: u64,
            generation: u64,
        },
        PeerFailed {
            id: u64,
            generation: u64,
            message: String,
        },
    }
    enum NativeCommand {
        Configure(VideoSettings),
        Start(
            ScreenConfig,
            mpsc::Sender<Arc<mac::EncodedPacket>>,
            Arc<AtomicBool>,
            Arc<Telemetry>,
            Arc<AtomicU64>,
        ),
    }
    enum NativeEvent {
        Display(u32),
        Prepared(Result<()>),
        Ended(Result<()>),
        Progress(String),
    }
    struct Peer {
        connection: quinn::Connection,
        controls: mpsc::Sender<HostMessage>,
        packets: Option<bridge_core::fanout::Subscriber<mac::EncodedPacket>>,
        sender: Option<tokio::task::JoinHandle<()>>,
        debug: bool,
        active: bool,
        telemetry: Arc<Telemetry>,
    }
    impl Drop for Peer {
        fn drop(&mut self) {
            self.packets = None;
            if let Some(task) = self.sender.take() {
                task.abort();
            }
        }
    }
    fn pin_for(shared:&Arc<Mutex<State>>,id:u32)->Option<String>{let s=shared.lock().ok()?;if s.sessions.sessions.iter().any(|session|session.contract.screens.iter().any(|screen|screen.screen_id==id)){return None;}s.screens.iter().find(|s|s.screen_id==id&&!s.removing).map(|s|s.pin.clone())}
    fn config(shared: &Arc<Mutex<State>>, id: u32) -> Result<ScreenConfig> {
        let state = shared
            .lock()
            .map_err(|_| anyhow::anyhow!("Host state unavailable"))?;
        let s = state
            .screens
            .iter()
            .find(|s| s.screen_id == id && !s.removing)
            .ok_or_else(|| anyhow::anyhow!("Screen removed"))?;
        Ok(ScreenConfig {
            screen_id: id,
            name: s.name.clone(),
            revision: s.revision,
            generation: s.generation,
            settings: s.settings.clone(),
            codec: s
                .settings
                .preferred_codec
                .or(s.codec)
                .unwrap_or(CodecOption {
                    codec: BridgeCodec::H264,
                    acceleration: Acceleration::Hardware,
                }),
            instrumented: s.instrumented(),
        })
    }
    fn peer_update(
        shared: &Arc<Mutex<State>>,
        screen_id: u32,
        id: u64,
        name: Option<String>,
        status: &str,
        telemetry: Arc<Telemetry>,
    ) {
        if let Ok(mut state) = shared.lock() {
            if let Some(s) = state.screens.iter_mut().find(|s| s.screen_id == screen_id) {
                if let Some(name) = name {
                    s.viewers.push(ViewerPeer {
                        id,
                        name,
                        status: status.into(),
                        telemetry,
                    });
                } else if let Some(p) = s.viewers.iter_mut().find(|p| p.id == id) {
                    p.status = status.into();
                }
            }
        }
    }
    fn peer_remove(shared: &Arc<Mutex<State>>, screen_id: u32, id: u64) {
        if let Ok(mut s) = shared.lock() {
            if let Some(screen) = s.screens.iter_mut().find(|s| s.screen_id == screen_id) {
                screen.viewers.retain(|p| p.id != id);
            }
        }
    }
    fn code(message: &str) -> FailureCode {
        if message.contains("API symbols unavailable") {
            FailureCode::ApiUnavailable
        } else if message.contains("cleanup") || message.contains("callbacks stopped") {
            FailureCode::Cleanup
        } else if message.contains("permission") || message.contains("Screen Recording") {
            FailureCode::PermissionDenied
        } else if message.contains("no frame") || message.contains("First-frame") {
            FailureCode::FirstFrameTimeout
        } else if message.contains("create")
            || message.contains("stream creation")
            || message.contains("virtual display")
        {
            FailureCode::CaptureCreate
        } else if message.contains("encoder") || message.contains("VideoToolbox") {
            FailureCode::Encoder
        } else {
            FailureCode::CaptureStart
        }
    }
    fn failure(screen: &ScreenConfig, message: String) -> HostMessage {
        HostMessage::Failure(Failure {
            code: code(&message),
            scope: FailureScope::Video,
            screen_id: screen.screen_id,
            generation: screen.generation,
            request_id: None,
            retryable: true,
            origin: "host capture".into(),
            message,
        })
    }
    fn notify(peers: &mut HashMap<u64, Peer>, message: HostMessage) {
        // A congested control reader is disconnected individually, never blocks
        // the source actor or causes unbounded queued status data.
        peers.retain(|_, p| {
            if p.controls.try_send(message.clone()).is_err() {
                p.connection.close(2u32.into(), b"control consumer stalled");
                false
            } else {
                true
            }
        });
    }
    #[cfg(not(test))]
    fn native_worker(
        id: u32,
        settings: VideoSettings,
        position:Option<bridge_core::sessions::DisplayPosition>,
        rx: native_mpsc::Receiver<NativeCommand>,
        events: mpsc::Sender<NativeEvent>,
    ) {
        let mut display: Option<(mac::VirtualDisplay, u32)> = None;
        let prepare = |display: &mut Option<(mac::VirtualDisplay, u32)>,
                       settings: &VideoSettings|
         -> Result<()> {
            if !mac::capture_allowed() {
                bail!(
                    "Screen Recording permission not granted for this executable. Grant access, restart it and retry."
                );
            }
            if let Some((owner, _)) = display {
                owner.update(settings)?;
            } else {
                *display = Some(mac::VirtualDisplay::new(settings)?);
                if let Some((_,display_id))=display{let _=events.blocking_send(NativeEvent::Display(*display_id));if let Some(position)=position{if let Err(error)=mac::restore_display_position(*display_id,position){let _=events.blocking_send(NativeEvent::Progress(format!("Display arrangement restoration unavailable: {error}")));}}}

            }
            Ok(())
        };
        let _ = events.blocking_send(NativeEvent::Prepared(prepare(&mut display, &settings)));
        while let Ok(cmd) = rx.recv() {
            match cmd {
                NativeCommand::Configure(settings) => {
                    let _ = events
                        .blocking_send(NativeEvent::Prepared(prepare(&mut display, &settings)));
                }
                NativeCommand::Start(config, packets, stop, telemetry, dropped) => {
                    let result = prepare(&mut display, &config.settings).and_then(|_| {
                        let display_id = display
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("Display not prepared"))?
                            .1;
                        let progress = |msg: &str| {
                            let _ = events.blocking_send(NativeEvent::Progress(msg.into()));
                        };
                        // Capture's packet ownership stays bounded. The bridge only
                        // shares encoded immutable packets; no display is per-peer.
                        let (tx, mut rx) = mpsc::channel(4);
                        let forward = std::thread::spawn(move || {
                            while let Some(packet) = rx.blocking_recv() {
                                if packets.blocking_send(Arc::new(packet)).is_err() {
                                    break;
                                }
                            }
                        });
                        let result = if config.instrumented {
                            mac::capture_display::<true>(
                                display_id,
                                id,
                                config.generation,
                                config.settings,
                                config.codec,
                                tx,
                                stop,
                                dropped,
                                telemetry,
                                progress,
                            )
                        } else {
                            mac::capture_display::<false>(
                                display_id,
                                id,
                                config.generation,
                                config.settings,
                                config.codec,
                                tx,
                                stop,
                                dropped,
                                telemetry,
                                progress,
                            )
                        };
                        let _ = forward.join();
                        result
                    });
                    let _ = events.blocking_send(NativeEvent::Ended(result));
                }
            }
        }
        // display drops only here, after capture has returned and callbacks stopped.
    }
    #[cfg(test)]
    #[allow(unused_variables)]
    fn native_worker(
        id: u32,
        _settings: VideoSettings,
        _position:Option<bridge_core::sessions::DisplayPosition>,
        rx: native_mpsc::Receiver<NativeCommand>,
        events: mpsc::Sender<NativeEvent>,
    ) {
        let _ = events.blocking_send(NativeEvent::Prepared(Ok(())));
        while let Ok(command) = rx.recv() {
            match command {
                NativeCommand::Configure(_) => {
                    let _ = events.blocking_send(NativeEvent::Prepared(Ok(())));
                }
                NativeCommand::Start(config, packets, stop, _telemetry, _dropped) => {
                    if config.settings.bitrate_kbps == 7777 {
                        let _ = events.blocking_send(NativeEvent::Ended(Err(anyhow::anyhow!(
                            "Screen Recording permission denied (injected)"
                        ))));
                        continue;
                    }
                    let mut sequence = 0;
                    while !stop.load(Ordering::Acquire) {
                        sequence += 1;
                        let packet = mac::EncodedPacket {
                            header: bridge_core::protocol::MediaHeader {
                                screen_id: id,
                                generation: config.generation,
                                kind: bridge_core::protocol::MediaKind::Video,
                                sequence,
                                timestamp_us: now_us(),
                                keyframe: sequence % 5 == 1,
                                width: config.settings.width,
                                height: config.settings.height,
                                sample_rate: 0,
                                channels: 0,
                            },
                            bytes: vec![1, 2, 3],
                            trace: None,
                            queued_us: 0,
                        };
                        if packets.blocking_send(Arc::new(packet)).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = events.blocking_send(NativeEvent::Ended(Ok(())));
                }
            }
        }
    }
    fn start_sender(
        id: u64,
        screen: ScreenConfig,
        peer: &mut Peer,
        actor: mpsc::Sender<ScreenCommand>,
    ) {
        peer.packets = None;
        if let Some(t) = peer.sender.take() {
            t.abort();
        }
        let (tx, rx) = mpsc::channel(PEER_QUEUE);
        peer.packets = Some(bridge_core::fanout::Subscriber::new(tx));
        let dropped = peer.packets.as_ref().unwrap().dropped.clone();
        let connection = peer.connection.clone();
        let telemetry = peer.telemetry.clone();
        let debug = screen.instrumented;
        peer.sender = Some(tokio::spawn(async move {
            let result = if debug {
                send_video::<true>(connection, screen.clone(), rx, telemetry, dropped).await
            } else {
                send_video::<false>(connection, screen.clone(), rx, telemetry, dropped).await
            };
            if let Err(error) = result {
                let _ = actor
                    .send(ScreenCommand::PeerFailed {
                        id,
                        generation: screen.generation,
                        message: format!("{error:#}"),
                    })
                    .await;
            }
        }));
    }
    async fn send_video<const DEBUG: bool>(
        connection: quinn::Connection,
        screen: ScreenConfig,
        mut packets: mpsc::Receiver<Arc<mac::EncodedPacket>>,
        telemetry: Arc<Telemetry>,
        dropped: Arc<AtomicU64>,
    ) -> Result<()> {
        let mut stream = connection.open_uni().await?;
        send_json(
            &mut stream,
            &MediaStreamHello {
                version: VERSION,
                screen_id: screen.screen_id,
                generation: screen.generation,
                lane: MediaLane::Video,
                video: Some(bridge_core::protocol::VideoFormat::default()),
            },
        )
        .await?;
        let mut bytes = 0u64;
        let mut frames = 0u64;
        let mut last = tokio::time::Instant::now();
        let mut previous = 0u64;
        while let Some(packet) = packets.recv().await {
            let write = async {
                if DEBUG {
                    let start = now_us();
                    let trace = packet
                        .trace
                        .as_ref()
                        .map(|t| bridge_core::telemetry::TraceToken {
                            sequence: t.sequence,
                            capture_us: t.capture_us,
                            offset_us: t.offset_us,
                            uncertainty_us: t.uncertainty_us,
                            telemetry: telemetry.clone(),
                        });
                    let trace = trace.as_ref();
                    if let Some(t) = trace {
                        t.mark(
                            Stage::SendQueue,
                            packet.queued_us,
                            Some(start),
                            Outcome::Complete,
                            "",
                        );
                        t.mark(Stage::Write, start, None, Outcome::Pending, "");
                    }
                    send_debug_media(
                        &mut stream,
                        &packet.header,
                        trace.map(|t| t.capture_us),
                        start,
                        &packet.bytes,
                    )
                    .await?;
                    if let Some(t) = trace {
                        t.mark(Stage::Write, start, Some(now_us()), Outcome::Complete, "");
                    }
                } else {
                    send_media(&mut stream, &packet.header, &packet.bytes).await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::time::timeout(Duration::from_secs(3), write)
                .await
                .map_err(|_| anyhow::anyhow!("Viewer video write stalled"))??;
            bytes += packet.bytes.len() as u64;
            frames += 1;
            if DEBUG && last.elapsed() >= Duration::from_secs(1) {
                let elapsed = last.elapsed().as_secs_f64();
                let transport = connection.stats();
                telemetry.stats(StreamStats {
                    at_us: now_us(),
                    frames,
                    fps: (frames - previous) as f64 / elapsed,
                    bitrate_kbps: bytes as f64 * 8.0 / 1000.0 / elapsed,
                    rtt_us: connection.rtt().as_micros() as u64,
                    lost_packets: transport.path.lost_packets,
                    dropped: dropped.load(Ordering::Relaxed),
                    send_queue: Some((packets.max_capacity() - packets.capacity()) as u32),
                    ..Default::default()
                });
                bytes = 0;
                previous = frames;
                last = tokio::time::Instant::now();
            }
        }
        stream.finish()?;
        Ok(())
    }
    async fn screen_actor(
        id: u32,
        shared: Arc<Mutex<State>>,
        mut commands: mpsc::Receiver<ScreenCommand>,
        handle: ScreenHandle,
    ) {
        let Ok(mut screen) = config(&shared, id) else {
            return;
        };
        let (native_tx, native_rx) = native_mpsc::sync_channel::<NativeCommand>(2);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let settings = screen.settings.clone();
        let position=shared.lock().ok().and_then(|s|s.screens.iter().find(|s|s.screen_id==id).and_then(|s|s.display_position));
        let native = std::thread::spawn(move || native_worker(id, settings, position, native_rx, event_tx));
        let (packet_tx, mut packet_rx) = mpsc::channel::<Arc<mac::EncodedPacket>>(4);
        let mut peers = HashMap::<u64, Peer>::new();
        let mut running = false;
        let mut pending = false;
        let mut removed = false;
        let mut ready = false;
        let mut began: Option<Instant> = None;
        let mut stopping: Option<Instant> = None;
        let mut quarantined = false;
        let mut stop = Arc::new(AtomicBool::new(false));
        let mut last_failure: Option<String> = None;
        let dropped = Arc::new(AtomicU64::new(0));
        let mut diagnostics_timer = tokio::time::interval(Duration::from_millis(500));
        diagnostics_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if pending && !running && !removed {
                screen = match config(&shared, id) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                if let Ok(mut s) = shared.lock() {
                    if let Some(s) = s.screens.iter_mut().find(|s| s.screen_id == id) {
                        s.generation = s.generation.saturating_add(1);
                        s.telemetry = Telemetry::new();
                        screen.generation = s.generation;
                    }
                }
                let telemetry = shared
                    .lock()
                    .ok()
                    .and_then(|s| {
                        s.screens
                            .iter()
                            .find(|s| s.screen_id == id)
                            .map(|s| s.telemetry.clone())
                    })
                    .unwrap_or_else(Telemetry::new);
                screen.instrumented = shared
                    .lock()
                    .map(|s| {
                        s.screens
                            .iter()
                            .any(|s| s.screen_id == id && s.stats_view.open)
                    })
                    .unwrap_or(false)
                    || peers.values().any(|p| p.debug);
                for (&peer_id, peer) in &mut peers {
                    if !peer.active {
                        continue;
                    }
                    peer.telemetry = Telemetry::new();
                    if let Ok(mut state) = shared.lock() {
                        if let Some(s) = state.screens.iter_mut().find(|s| s.screen_id == id) {
                            if let Some(p) = s.viewers.iter_mut().find(|p| p.id == peer_id) {
                                p.telemetry = peer.telemetry.clone();
                            }
                        }
                    }
                    let _ = peer.controls.try_send(HostMessage::ScreenSettingsApplied {
                        request_id: None,
                        screen: screen.clone(),
                    });
                    start_sender(peer_id, screen.clone(), peer, handle.tx.clone());
                }
                if peers.values().any(|p| p.active) {
                    stop = Arc::new(AtomicBool::new(false));
                    if native_tx
                        .try_send(NativeCommand::Start(
                            screen.clone(),
                            packet_tx.clone(),
                            stop.clone(),
                            telemetry,
                            dropped.clone(),
                        ))
                        .is_err()
                    {
                        last_failure = Some("Capture command queue is full".into());
                        notify(&mut peers, failure(&screen, last_failure.clone().unwrap()));
                    } else {
                        running = true;
                        began = Some(Instant::now());
                        stopping = None;
                        ready = false;
                        last_failure = None;
                        notify(
                            &mut peers,
                            HostMessage::StreamStatus {
                                screen_id: id,
                                generation: screen.generation,
                                lane: MediaLane::Video,
                                state: MediaState::Starting,
                                message: "Starting shared video capture".into(),
                            },
                        );
                    }
                } else {
                    let _ = native_tx.try_send(NativeCommand::Configure(screen.settings.clone()));
                }
                pending = false;
            }
            tokio::select! {
                _=diagnostics_timer.tick()=>{
                    if running && stop.load(Ordering::Acquire) && stopping.is_none(){stopping=Some(Instant::now());}
                    if running && !ready && began.is_some_and(|t|t.elapsed()>Duration::from_secs(15)) && !stop.load(Ordering::Acquire){
                        stop.store(true,Ordering::Release);pending=false;
                        notify(&mut peers,failure(&screen,"First-frame startup timeout; stopping capture".into()));
                        set_screen_message(&shared,id,"First-frame startup timeout; stopping capture");
                    }
                    if stopping.is_some_and(|t|t.elapsed()>Duration::from_secs(10)){
                        quarantined=true;mac::quarantine_capture();
                        notify(&mut peers,failure(&screen,"Native capture cleanup timed out; restart host before reusing resources".into()));
                        set_screen_message(&shared,id,"Cleanup unconfirmed · resources retained · restart host");break;
                    }
                    if screen.instrumented{
                        let source=shared.lock().ok().and_then(|s|s.screens.iter().find(|s|s.screen_id==id).map(|s|s.telemetry.clone()));
                        if let Some(source)=source{let batch=source.drain(id,screen.generation);for peer in peers.values(){for event in &batch.events{peer.telemetry.trace(event.clone());}}}
                    }
                },
                command=commands.recv()=>match command{
                    Some(ScreenCommand::Join{id:peer_id,connection,codecs,controls,reply})=>{
                        if removed || peers.len()>=bridge_core::protocol::MAX_VIEWERS_PER_SCREEN{let _=reply.send(Err(anyhow::anyhow!("Screen unavailable or viewer limit reached")));continue;}
                        if !codecs.iter().any(|c|c.codec==screen.codec.codec){let _=reply.send(Err(anyhow::anyhow!("Viewer cannot decode this screen's active codec")));continue;}
                        let telemetry=Telemetry::new();peer_update(&shared,id,peer_id,Some(connection.remote_address().to_string()),"Attached",telemetry.clone());
                        let mut peer=Peer{connection,controls,packets:None,sender:None,debug:false,active:true,telemetry};
                        if running{start_sender(peer_id,screen.clone(),&mut peer,handle.tx.clone());}
                        peers.insert(peer_id,peer);let _=reply.send(Ok(screen.clone()));if !running{pending=true;}
                    }
                    Some(ScreenCommand::Leave(peer))=>{peers.remove(&peer);peer_remove(&shared,id,peer);if peers.is_empty(){stop.store(true,Ordering::Release);}}
                    Some(ScreenCommand::Disconnect(peer))=>{if let Some(p)=peers.remove(&peer){p.connection.close(0u32.into(),b"host disconnected viewer");}peer_remove(&shared,id,peer);if peers.is_empty(){stop.store(true,Ordering::Release);}}
                    Some(ScreenCommand::Settings{peer,request,mut settings,revision})=>{
                        settings.audio=false;
                        let result=settings.validate().and_then(|_|{
                            if settings.preferred_codec.is_some_and(|c|!mac::available_codecs().contains(&c)){bail!("Requested encoder is not available on this host");}
                            let mut s=shared.lock().map_err(|_|anyhow::anyhow!("Host state unavailable"))?;
                            let entry=s.screens.iter_mut().find(|s|s.screen_id==id).ok_or_else(||anyhow::anyhow!("Screen removed"))?;
                            if revision.is_some_and(|r|r<=entry.revision){bail!("Settings revision is stale");}
                            entry.revision=revision.unwrap_or(entry.revision.saturating_add(1));entry.settings=settings.clone();if s.navigation.selected==Some(id){s.settings=settings;}Ok(())
                        });
                        match result{Ok(())=>{notify(&mut peers,HostMessage::StreamStatus{screen_id:id,generation:screen.generation,lane:MediaLane::Video,state:MediaState::Restarting,message:"Shared source settings changed; restarting all subscribers".into()});stop.store(true,Ordering::Release);pending=true;}
                            Err(error)=>{if peer.is_none(){if let Ok(mut state)=shared.lock(){state.settings=screen.settings.clone();}}if let Some(p)=peer.and_then(|id|peers.get(&id)){let _=p.controls.try_send(HostMessage::Error{request_id:request,screen_id:Some(id),message:error.to_string()});}else{set_screen_message(&shared,id,&error.to_string());}}}
                    }
                    Some(ScreenCommand::Diagnostics{peer,enabled})=>{
                        if let Some(p)=peer.and_then(|id|peers.get_mut(&id)){p.debug=enabled;}
                        let instrumented=shared.lock().map(|s|s.screens.iter().any(|s|s.screen_id==id&&s.stats_view.open)).unwrap_or(false)||peers.values().any(|p|p.debug);
                        if let Ok(mut state)=shared.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.screen_id==id){s.viewer_stats=peers.values().any(|p|p.debug);}}
                        if instrumented!=screen.instrumented{stop.store(true,Ordering::Release);pending=true;}
                    }
                    Some(ScreenCommand::Burst(peer))=>{if let Ok(s)=shared.lock(){if let Some(s)=s.screens.iter().find(|s|s.screen_id==id){s.telemetry.burst();}}if let Some(p)=peer.and_then(|id|peers.get(&id)){p.telemetry.burst();}}
                    Some(ScreenCommand::VideoStopped{id:peer,generation})=>{
                        if generation==screen.generation{if let Some(p)=peers.get_mut(&peer){p.active=false;p.packets=None;if let Some(task)=p.sender.take(){task.abort();}peer_update(&shared,id,peer,None,"Video stopped; ready to retry",p.telemetry.clone());}
                        if peers.values().all(|p|p.packets.is_none()){stop.store(true,Ordering::Release);}}
                    }
                    Some(ScreenCommand::PeerFailed{id:peer,generation,message})=>{if generation==screen.generation{if let Some(p)=peers.remove(&peer){let _=p.controls.try_send(HostMessage::Failure(Failure{code:FailureCode::Transport,scope:FailureScope::Video,screen_id:id,generation,request_id:None,retryable:true,origin:"host sender".into(),message}));}peer_remove(&shared,id,peer);if peers.is_empty(){stop.store(true,Ordering::Release);}}}
                    Some(ScreenCommand::Remove)|None=>{if let Ok(mut state)=shared.lock(){if let Some(s)=state.screens.iter_mut().find(|s|s.screen_id==id){s.removing=true;s.status="Stopping capture and removing display…".into();}}removed=true;stop.store(true,Ordering::Release);notify(&mut peers,HostMessage::ScreenRemoved{request_id:None,screen_id:id});peers.clear();if !running{break;}}
                },
                event=event_rx.recv()=>match event{
                    Some(NativeEvent::Display(display_id))=>{if let Ok(mut s)=shared.lock(){if let Some(screen)=s.screens.iter_mut().find(|s|s.screen_id==id){screen.display_id=Some(display_id);}}}
                    Some(NativeEvent::Prepared(result))=>{match result{Ok(())=>{if !running{set_screen_message(&shared,id,"Prepared · waiting for viewers");}},Err(error)=>{let detail=format!("{error:#}");last_failure=Some(detail.clone());set_screen_message(&shared,id,&detail);if !peers.is_empty(){notify(&mut peers,failure(&screen,detail));}}}}
                    Some(NativeEvent::Progress(message))=>{set_screen_generation_message(&shared,id,screen.generation,&message);notify(&mut peers,HostMessage::StreamStatus{screen_id:id,generation:screen.generation,lane:MediaLane::Video,state:MediaState::WaitingForFirstFrame,message});}
                    Some(NativeEvent::Ended(result))=>{
                        running=false;began=None;stopping=None;for p in peers.values_mut(){p.packets=None;if let Some(task)=p.sender.take(){task.abort();}}
                        if let Err(error)=result{let detail=format!("{error:#}");last_failure=Some(detail.clone());set_screen_message(&shared,id,&detail);notify(&mut peers,failure(&screen,detail));}
                        else if !pending{set_screen_message(&shared,id,"Prepared · video stopped");notify(&mut peers,HostMessage::StreamStatus{screen_id:id,generation:screen.generation,lane:MediaLane::Video,state:MediaState::Stopped,message:"Video stopped".into()});}
                        if removed{break;}
                    }
                    None=>{notify(&mut peers,failure(&screen,"Native capture worker exited".into()));break;}
                },
                packet=packet_rx.recv()=>{if let Some(packet)=packet{
                    if packet.header.generation!=screen.generation||pending||removed{continue;}
                    if !ready{ready=true;set_screen_message(&shared,id,"Streaming shared video");notify(&mut peers,HostMessage::StreamStatus{screen_id:id,generation:screen.generation,lane:MediaLane::Video,state:MediaState::Ready,message:"Shared video frames are flowing".into()});}
                    for (&peer_id,p) in &mut peers{
                        if let Some(tx)=p.packets.as_mut(){match tx.publish(packet.clone(),packet.header.keyframe){
                            bridge_core::fanout::Delivery::Overflow=>peer_update(&shared,id,peer_id,None,"Resynchronizing at next keyframe",p.telemetry.clone()),
                            bridge_core::fanout::Delivery::Queued if packet.header.keyframe=>peer_update(&shared,id,peer_id,None,"Streaming",p.telemetry.clone()),
                            bridge_core::fanout::Delivery::Closed=>{},
                            _=>{},
                        }}
                    }
                    if let Ok(mut s)=shared.lock(){if let Some(s)=s.screens.iter_mut().find(|s|s.screen_id==id){s.frames+=1;s.bytes+=packet.bytes.len() as u64;s.dropped=dropped.load(Ordering::Relaxed);}}
                }},
            }
            if peers.is_empty() && last_failure.is_some() {
                pending = false;
            }
        }
        stop.store(true, Ordering::Release);
        drop(packet_rx);
        drop(native_tx);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !native.is_finished() && !quarantined && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if native.is_finished() {
            let _ = native.join();
            if removed {
                remove_host_screen(&shared, id);
            }
        } else {
            mac::quarantine_capture();
            set_screen_message(
                &shared,
                id,
                "Cleanup unconfirmed · resources retained · restart host",
            );
        }
    }
    async fn serve_peer(
        connection: quinn::Connection,
        id: u64,
        host_id: String,
        shared: Arc<Mutex<State>>,
        actors: Arc<Mutex<HashMap<u32, ScreenHandle>>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let authenticated=tokio::time::timeout(Duration::from_secs(20),authenticate_host_target(&connection,|id|pin_for(&shared,id),|login|{
            shared.lock().ok()?.sessions.sessions.iter().find(|s|s.contract.username==login.username&&login.session_id.as_ref().is_none_or(|id|*id==s.contract.session_id)&&login.host_id.as_ref().is_none_or(|id|*id==s.contract.host_id)).cloned()
        })).await.map_err(|_|anyhow::anyhow!("Authentication timed out"))??;
        let(session_id,stable_viewer,screen_ids)=match authenticated {
            AuthenticatedTarget::Screen(id)=>(None,None,vec![id]),
            AuthenticatedTarget::Session{session_id,viewer_id}=>{
                let ids=shared.lock().ok().and_then(|s|s.sessions.sessions.iter().find(|s|s.contract.session_id==session_id).map(|s|s.contract.screens.iter().map(|s|s.screen_id).collect())).ok_or_else(||anyhow::anyhow!("Session removed"))?;
                (Some(session_id),Some(viewer_id),ids)
            }
        };
        let(mut send,mut recv)=tokio::time::timeout(Duration::from_secs(8),connection.accept_bi()).await??;
        let greeting:ClientMessage=tokio::time::timeout(Duration::from_secs(8),receive_json(&mut recv)).await??;
        let (hello,preferences)=match greeting {
            ClientMessage::Hello(hello) if session_id.is_none()=>(hello,None),
            ClientMessage::SessionHello{hello,viewer_id,preferences} if Some(&viewer_id)==stable_viewer.as_ref()=>{preferences.validate()?;(hello,Some(preferences))},
            _=>bail!("Invalid authenticated session greeting"),
        };
        if hello.version!=VERSION{bail!("Protocol mismatch");}
        if let (Some(session_id),Some(viewer),Some(mut preferences))=(&session_id,&stable_viewer,preferences){
            let mut state=shared.lock().map_err(|_|anyhow::anyhow!("Host state unavailable"))?;
            let session=state.sessions.sessions.iter_mut().find(|s|s.contract.session_id==*session_id).ok_or_else(||anyhow::anyhow!("Session removed"))?;
            preferences.screens.retain(|p|session.contract.screens.iter().any(|screen|screen.screen_id==p.screen_id));
            session.viewers.retain(|v|v.viewer_id!=*viewer);if session.viewers.len()>=128 {bail!("Registered viewer limit reached");}
            session.viewers.push(bridge_core::sessions::ViewerRegistration{viewer_id:viewer.clone(),preferences});
        }
        let(controls,mut outgoing)=mpsc::channel(64);
        let mut attached=HashMap::<u32,ScreenHandle>::new();let mut screens=Vec::new();
        let candidates=screen_ids.iter().map(|id|get_actor(&actors,*id).map(|actor|(*id,actor)).ok_or_else(||anyhow::anyhow!("Screen removed during authentication"))).collect::<Result<Vec<_>>>()?;
        for (screen_id,actor) in candidates {
            let(reply,accepted)=oneshot::channel();
            let result=async{actor.tx.send(ScreenCommand::Join{id,connection:connection.clone(),codecs:hello.codecs.clone(),controls:controls.clone(),reply}).await?;Ok::<_,anyhow::Error>(accepted.await??)}.await;
            match result {Ok(screen)=>{attached.insert(screen_id,actor);screens.push(screen);},Err(error)=>{for actor in attached.values(){let _=actor.tx.send(ScreenCommand::Leave(id)).await;}return Err(error);}}
        }
        if session_id.is_none(){if let Some(settings)=hello.initial_settings{if let Some(actor)=attached.values().next(){let _=actor.tx.send(ScreenCommand::Settings{peer:Some(id),request:None,settings,revision:None}).await;}}}
        let accepted_message=if let Some(session_id)=&session_id {
            let contract=shared.lock().ok().and_then(|s|s.sessions.sessions.iter().find(|s|s.contract.session_id==*session_id).map(|s|s.contract.clone())).ok_or_else(||anyhow::anyhow!("Session removed"))?;
            HostMessage::PersistentSessionAccepted{contract,screens,viewer_id:id}
        }else{HostMessage::SessionAccepted{host_id,viewer_id:id,screen:screens.remove(0)}};
        if let Err(error)=send_control(&mut send,&accepted_message).await{for actor in attached.values(){let _=actor.tx.send(ScreenCommand::Leave(id)).await;}return Err(error);}
        let diagnostics_shared = shared.clone();
        let diagnostics_send = tokio::spawn(bridge_core::telemetry_transport::send_batches(
            connection.clone(),
            move || {
                diagnostics_shared.lock().map(|s|s.screens.iter().filter_map(|screen|screen.viewers.iter().find(|p|p.id==id).map(|peer|(screen.screen_id,screen.generation,peer.telemetry.clone()))).collect()).unwrap_or_default()
            },
        ));
        let diagnostics_connection = connection.clone();
        let diagnostics_state = shared.clone();

        let diagnostics_receive = tokio::spawn(async move {
            while let Ok(mut stream) = diagnostics_connection.accept_uni().await {
                let hello: Result<MediaStreamHello> =
                    tokio::time::timeout(Duration::from_secs(3), receive_json(&mut stream))
                        .await
                        .map_err(|_| anyhow::anyhow!("Diagnostics hello timed out"))
                        .and_then(|r| r);
                if !hello.is_ok_and(|h| h.version == VERSION && h.lane == MediaLane::Diagnostics) {
                    let _ = stream.stop(1u32.into());
                    continue;
                }
                let state = diagnostics_state.clone();

                let _ =
                    bridge_core::telemetry_transport::receive_batches(stream, move |batch| {
                        if let Ok(s) = state.lock() {
                            if let Some(screen) = s.screens.iter().find(|s| {
                                s.screen_id == batch.screen_id && s.generation == batch.generation
                            }) {
                                if let Some(peer) = screen.viewers.iter().find(|p| p.id == id) {
                                    peer.telemetry.peer_lost(batch.lost_events);
                                    if let Some(stats) = batch.stats {
                                        peer.telemetry.peer_stats(stats);
                                    }
                                    for event in batch.events {
                                        peer.telemetry.ingest(event);
                                    }
                                }
                            }
                        }
                    })
                    .await;
            }
        });
        let (mut incoming, reader) = control_reader::<_, ClientMessage>(recv);
        let mut contract_timer=tokio::time::interval(Duration::from_secs(1));
        let mut last_contract=String::new();
        let mut detached=std::collections::HashSet::new();
        let result:Result<()>=async{loop{tokio::select!{
            _=shutdown.changed()=>break,
            _=connection.closed()=>break,
            _=contract_timer.tick(),if session_id.is_some()=>{
                let contract=shared.lock().ok().and_then(|s|s.sessions.sessions.iter().find(|s|Some(&s.contract.session_id)==session_id.as_ref()).map(|s|s.contract.clone()));
                let Some(contract)=contract else {bail!("Session deleted");};
                let snapshot=bridge_core::sessions::contract_snapshot(&contract)?;
                if snapshot!=last_contract {
                    send_control(&mut send,&HostMessage::SessionUpdated(contract.clone())).await?;
                    let mut new_screens=Vec::new();
                    for saved in &contract.screens {if !attached.contains_key(&saved.screen_id)&&!detached.contains(&saved.screen_id){
                        if let Some(actor)=get_actor(&actors,saved.screen_id){let(reply,accepted)=oneshot::channel();actor.tx.send(ScreenCommand::Join{id,connection:connection.clone(),codecs:hello.codecs.clone(),controls:controls.clone(),reply}).await?;
                            let screen=accepted.await??;attached.insert(saved.screen_id,actor);new_screens.push(screen);
                        }
                    }}
                    for screen in new_screens{send_control(&mut send,&HostMessage::ScreenAdded{request_id:None,screen}).await?;}
                    let removed=attached.keys().copied().filter(|id|!contract.screens.iter().any(|s|s.screen_id==*id)).collect::<Vec<_>>();
                    for screen_id in removed{if let Some(actor)=attached.remove(&screen_id){let _=actor.tx.send(ScreenCommand::Leave(id)).await;}send_control(&mut send,&HostMessage::ScreenRemoved{request_id:None,screen_id}).await?;}
                    last_contract=snapshot;
                }
            }
            message=outgoing.recv()=>{let Some(message)=message else{break};let removed=match &message{HostMessage::ScreenRemoved{screen_id,..}=>Some(*screen_id),_=>None};send_control(&mut send,&message).await?;if let Some(screen_id)=removed{attached.remove(&screen_id);if session_id.is_none(){break;}}}
            message=incoming.recv()=>match message{
                Some(Ok(ClientMessage::ChangeScreenSettings(change))) if attached.contains_key(&change.screen_id)=>{attached[&change.screen_id].tx.send(ScreenCommand::Settings{peer:Some(id),request:Some(change.request_id),settings:change.settings,revision:Some(change.revision)}).await?;}
                Some(Ok(ClientMessage::Diagnostics{screen_id,enabled})) if attached.contains_key(&screen_id)=>{attached[&screen_id].tx.send(ScreenCommand::Diagnostics{peer:Some(id),enabled}).await?;}
                Some(Ok(ClientMessage::TraceBurst{screen_id})) if attached.contains_key(&screen_id)=>{attached[&screen_id].tx.send(ScreenCommand::Burst(Some(id))).await?;}
                Some(Ok(ClientMessage::ClockProbe{t1}))=>{let t2=now_us();send_control(&mut send,&HostMessage::ClockReply{t1,t2,t3:now_us()}).await?;}
                Some(Ok(ClientMessage::VideoStopped{screen_id,generation})) if attached.contains_key(&screen_id)=>{attached[&screen_id].tx.send(ScreenCommand::VideoStopped{id,generation}).await?;}
                Some(Ok(ClientMessage::SessionPreferences(preferences))) if session_id.is_some()=>{
                    preferences.validate()?;
                    let mut s=shared.lock().map_err(|_|anyhow::anyhow!("Host state unavailable"))?;
                    if let Some(session)=s.sessions.sessions.iter_mut().find(|s|Some(&s.contract.session_id)==session_id.as_ref()){
                        if preferences.screens.iter().any(|screen|!session.contract.screens.iter().any(|s|s.screen_id==screen.screen_id)){bail!("Viewer preferences target another session");}
                        if let Some(viewer)=session.viewers.iter_mut().find(|v|Some(&v.viewer_id)==stable_viewer.as_ref()){viewer.preferences=preferences;}
                    }
                }
                Some(Ok(ClientMessage::RemoveScreen{request_id,screen_id})) if attached.contains_key(&screen_id)=>{detached.insert(screen_id);if let Some(actor)=attached.remove(&screen_id){let _=actor.tx.send(ScreenCommand::Leave(id)).await;}send_control(&mut send,&HostMessage::ScreenRemoved{request_id:Some(request_id),screen_id}).await?;}
                Some(Ok(ClientMessage::Stop))|None=>break,
                Some(Ok(_))=>{send_control(&mut send,&HostMessage::Error{request_id:None,screen_id:None,message:"Request is outside this session's screens".into()}).await?;}
                Some(Err(error))=>return Err(error),
            }
        }}Ok(())}.await;
        diagnostics_send.abort();
        diagnostics_receive.abort();
        let _ = diagnostics_send.await;
        let _ = diagnostics_receive.await;
        reader.abort();
        let _ = reader.await;
        for actor in attached.values(){let _=actor.tx.send(ScreenCommand::Leave(id)).await;}
        result
    }
    pub(super) fn run(
        shared: Arc<Mutex<State>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move{
            let endpoint=server_endpoint(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED),PORT))?;
            let host_id=shared.lock().map_err(|_|anyhow::anyhow!("Host state unavailable"))?.sessions.host_id.clone();
            let _host_advert=Advertiser::screen(&format!("AirBridge Host {}",&host_id[..8]),0,&host_id).map_err(|e|{set_message(&shared,format!("Host discovery unavailable: {e}"));e}).ok();
            let (ui_tx,mut ui)=mpsc::channel(64);if let Ok(mut s)=shared.lock(){s.session_commands=Some(ui_tx);}
            let actors=Arc::new(Mutex::new(HashMap::<u32,ScreenHandle>::new()));let mut tasks=JoinSet::new();let mut sessions=JoinSet::new();let mut adverts=HashMap::new();let mut next_peer=1u64;let mut admissions=HashMap::<IpAddr,(Instant,u32)>::new();
            let ids=shared.lock().map(|s|s.screens.iter().map(|s|s.screen_id).collect::<Vec<_>>()).unwrap_or_default();
            for id in ids{add_actor(id,&shared,&host_id,&actors,&mut tasks,&mut adverts);}
            set_message(&shared,"Ready · session accounts and temporary screen PINs");
            let power_state=shared.clone();let power=bridge_core::power::PowerController::new(move|notice|{if let Ok(mut s)=power_state.lock(){s.power_notice=notice;}});
            let mut power_timer=tokio::time::interval(Duration::from_millis(500));
            loop{tokio::select!{
                _=power_timer.tick()=>{if let Ok(mut s)=shared.lock(){if let Err(e)=persist_sessions(&mut s){s.message=format!("Session save failed: {e}");}}let active=shared.lock().map(|s|s.sessions.sessions.iter().any(|session|session.contract.keep_awake&&session.contract.screens.iter().any(|saved|s.screens.iter().any(|screen|screen.screen_id==saved.screen_id&&screen.status=="Streaming shared video"&&screen.viewers.iter().any(|peer|peer.status=="Streaming"))))).unwrap_or(false);power.set_active(active);}
                _=shutdown.changed()=>break,
                incoming=endpoint.accept()=>{let Some(incoming)=incoming else{break};admissions.retain(|_,(t,_)|t.elapsed()<Duration::from_secs(10));let ip=incoming.remote_address().ip();if !admissions.contains_key(&ip)&&admissions.len()>=128{incoming.refuse();continue;}let limit=admissions.entry(ip).or_insert((Instant::now(),0));if limit.1>=10{incoming.refuse();continue;}limit.1+=1;if sessions.len()>=64{incoming.refuse();continue;}let shared=shared.clone();let actors=actors.clone();let shutdown=shutdown.clone();let host_id=host_id.clone();let id=next_peer;next_peer+=1;sessions.spawn(async move{if let Ok(connection)=incoming.await{if let Err(error)=serve_peer(connection.clone(),id,host_id,shared.clone(),actors,shutdown).await{set_message(&shared,format!("Viewer {id}: {error:#}"));}connection.close(0u32.into(),b"viewer session ended");}});}
                Some(_)=sessions.join_next()=>{},
                Some(_)=tasks.join_next()=>{},
                command=ui.recv()=>match command{
                    Some(HostUiCommand::AddScreen(id))=>{add_actor(id,&shared,&host_id,&actors,&mut tasks,&mut adverts);}
                    Some(HostUiCommand::ChangeSettings(id,settings))=>{if let Some(a)=get_actor(&actors,id){let _=a.tx.send(ScreenCommand::Settings{peer:None,request:None,settings,revision:None}).await;}}
                    Some(HostUiCommand::RemoveScreen(id))=>{if let Some(a)=actors.lock().ok().and_then(|mut a|a.remove(&id)){let _=a.tx.send(ScreenCommand::Remove).await;}adverts.remove(&id);}
                    Some(HostUiCommand::Diagnostics(id,enabled))=>{if let Some(a)=get_actor(&actors,id){let _=a.tx.send(ScreenCommand::Diagnostics{peer:None,enabled}).await;}}
                    Some(HostUiCommand::Burst(id))=>{if let Some(a)=get_actor(&actors,id){let _=a.tx.send(ScreenCommand::Burst(None)).await;}}
                    Some(HostUiCommand::DisconnectPeer(screen,id))=>{if let Some(a)=get_actor(&actors,screen){let _=a.tx.send(ScreenCommand::Disconnect(id)).await;}}
None=>break,
                }
            }}
            let handles=actors.lock().map(|mut a|a.drain().map(|(_,v)|v).collect::<Vec<_>>()).unwrap_or_default();for a in handles{let _=a.tx.send(ScreenCommand::Remove).await;}
            endpoint.close(0u32.into(),b"host shutdown");while sessions.join_next().await.is_some(){}while tasks.join_next().await.is_some(){}Ok(())
        })
    }
    fn get_actor(actors: &Arc<Mutex<HashMap<u32, ScreenHandle>>>, id: u32) -> Option<ScreenHandle> {
        actors.lock().ok()?.get(&id).cloned()
    }
    fn add_actor(
        id: u32,
        shared: &Arc<Mutex<State>>,
        host_id: &str,
        actors: &Arc<Mutex<HashMap<u32, ScreenHandle>>>,
        tasks: &mut JoinSet<()>,
        adverts: &mut HashMap<u32, Advertiser>,
    ) {
        if get_actor(actors, id).is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel(64);
        let handle = ScreenHandle { tx };
        if let Ok(mut a) = actors.lock() {
            a.insert(id, handle.clone());
        }
        match Advertiser::screen(&format!("AirBridge {} Screen {id}", host_id), id, host_id) {
            Ok(a) => {
                adverts.insert(id, a);
            }
            Err(error) => {
                set_screen_message(shared, id, &format!("Discovery unavailable: {error}"))
            }
        }
        tasks.spawn(screen_actor(id, shared.clone(), rx, handle));
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        async fn pair() -> (
            quinn::Endpoint,
            quinn::Endpoint,
            quinn::Connection,
            quinn::Connection,
        ) {
            let h = server_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
            let c = bridge_core::security::client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
            let outgoing = c
                .connect(h.local_addr().unwrap(), "airbridge.local")
                .unwrap();
            let (v, s) = tokio::join!(outgoing, async { h.accept().await.unwrap().await.unwrap() });
            (h, c, s, v.unwrap())
        }
        async fn next_matching(
            rx: &mut mpsc::Receiver<HostMessage>,
            predicate: impl Fn(&HostMessage) -> bool,
        ) -> HostMessage {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let m = rx.recv().await.unwrap();
                    if predicate(&m) {
                        return m;
                    }
                }
            })
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn persistent_session_authenticates_once_and_detaches_one_screen(){
            tokio::time::timeout(Duration::from_secs(15),async{
                let mut state=initial_state(built_in_presets(),"123456".into());let second=add_host_screen(&mut state).unwrap();
                let saved=state.screens.iter().map(|s|bridge_core::sessions::SavedScreen{position:None,screen_id:s.screen_id,name:s.name.clone(),settings:s.settings.clone()}).collect();
                let session_id=state.sessions.create("alice".into(),"session-password",saved).unwrap();let host_id=state.sessions.host_id.clone();
                let shared=Arc::new(Mutex::new(state));assert!(pin_for(&shared,1).is_none());
                let actors=Arc::new(Mutex::new(HashMap::new()));let mut tasks=Vec::new();
                for id in [1,second]{let(tx,rx)=mpsc::channel(64);let handle=ScreenHandle{tx};actors.lock().unwrap().insert(id,handle.clone());tasks.push(tokio::spawn(screen_actor(id,shared.clone(),rx,handle)));}
                let(_h,_c,sender,viewer)=pair().await;let(shutdown,watch)=watch::channel(false);
                let peer=tokio::spawn(serve_peer(sender,17,host_id.clone(),shared.clone(),actors.clone(),watch));
                let stable_viewer=bridge_core::sessions::new_id().unwrap();let login=bridge_core::security::SessionLogin{username:"alice".into(),session_id:Some(session_id.clone()),host_id:Some(host_id),viewer_id:stable_viewer.clone()};
                let(auth_id,key)=bridge_core::security::authenticate_viewer_session(&viewer,&login,"session-password",None).await.unwrap();assert_eq!(auth_id,session_id);assert_eq!(key.0,shared.lock().unwrap().sessions.sessions[0].key.0);
                let(mut requests,mut replies)=viewer.open_bi().await.unwrap();
                send_control(&mut requests,&ClientMessage::SessionHello{hello:bridge_core::protocol::ClientHello{version:VERSION,codecs:mac::available_codecs(),initial_settings:None},viewer_id:stable_viewer,preferences:Default::default()}).await.unwrap();
                let accepted:HostMessage=receive_json(&mut replies).await.unwrap();let HostMessage::PersistentSessionAccepted{screens,contract,..}=accepted else{panic!("wrong acceptance")};assert_eq!(screens.len(),2);assert_eq!(contract.session_id,session_id);
                for policy in [bridge_core::sessions::ViewerLockPolicy::KeepAwake,bridge_core::sessions::ViewerLockPolicy::AllowIdle]{
                    {let mut state=shared.lock().unwrap();let contract=&mut state.sessions.sessions[0].contract;contract.viewer_lock_policy=policy;contract.revision+=1;}
                    loop{let message:HostMessage=receive_json(&mut replies).await.unwrap();if let HostMessage::SessionUpdated(contract)=message{if contract.viewer_lock_policy==policy{break;}}}
                }
                send_control(&mut requests,&ClientMessage::RemoveScreen{request_id:44,screen_id:1}).await.unwrap();
                loop{let message:HostMessage=receive_json(&mut replies).await.unwrap();if matches!(message,HostMessage::ScreenRemoved{request_id:Some(44),screen_id:1}){break;}}
                send_control(&mut requests,&ClientMessage::ClockProbe{t1:987}).await.unwrap();loop{let message:HostMessage=receive_json(&mut replies).await.unwrap();if matches!(message,HostMessage::ClockReply{t1:987,..}){break;}}
                assert!(shared.lock().unwrap().screens.iter().find(|s|s.screen_id==second).unwrap().viewers.iter().any(|v|v.id==17));
                send_control(&mut requests,&ClientMessage::ChangeScreenSettings(bridge_core::protocol::ChangeScreenSettingsRequest{request_id:55,screen_id:999,revision:1,settings:built_in_presets()[1].settings.clone()})).await.unwrap();
                loop{let message:HostMessage=receive_json(&mut replies).await.unwrap();if matches!(message,HostMessage::Error{..}){break;}}
                send_control(&mut requests,&ClientMessage::Stop).await.unwrap();peer.await.unwrap().unwrap();shutdown.send_replace(true);
                let handles=actors.lock().unwrap().values().cloned().collect::<Vec<_>>();for actor in handles{actor.tx.send(ScreenCommand::Remove).await.unwrap();}for task in tasks{task.await.unwrap();}
            }).await.unwrap();
        }
        #[tokio::test]
        async fn session_password_proof_rejects_wrong_secret_and_resumes_with_remembered_key(){
            let mut store=bridge_core::sessions::HostStore::new().unwrap();let id=store.create("account".into(),"correct-password",vec![]).unwrap();let session=store.sessions[0].clone();
            for valid in [false,true]{
                let(_h,_c,host,viewer)=pair().await;let value=session.clone();let host_auth=tokio::spawn(async move{bridge_core::security::authenticate_host_target(&host,|_|None,move |_|Some(value.clone())).await});
                let login=bridge_core::security::SessionLogin{username:"account".into(),session_id:Some(id.clone()),host_id:Some(store.host_id.clone()),viewer_id:bridge_core::sessions::new_id().unwrap()};
                let result=bridge_core::security::authenticate_viewer_session(&viewer,&login,if valid{""}else{"wrong-password"},valid.then_some(&session.key)).await;
                if valid{assert!(result.is_ok());assert!(matches!(host_auth.await.unwrap().unwrap(),AuthenticatedTarget::Session{..}));}else{assert!(result.is_err());viewer.close(1u32.into(),b"invalid proof");assert!(tokio::time::timeout(Duration::from_secs(2),host_auth).await.unwrap().unwrap().is_err());}
            }
        }
        #[tokio::test]
        async fn empty_and_single_character_session_passwords_authenticate(){
            tokio::time::timeout(Duration::from_secs(10),async{
                for password in ["","a"]{
                    let mut store=bridge_core::sessions::HostStore::new().unwrap();let id=store.create("user".into(),password,vec![]).unwrap();let session=store.sessions[0].clone();
                    let(_h,_c,host,viewer)=pair().await;
                    let host_auth=tokio::spawn(async move{bridge_core::security::authenticate_host_target(&host,|_|None,move |_|Some(session.clone())).await});
                    let login=bridge_core::security::SessionLogin{username:"user".into(),session_id:Some(id.clone()),host_id:Some(store.host_id),viewer_id:bridge_core::sessions::new_id().unwrap()};
                    let(authenticated,_)=bridge_core::security::authenticate_viewer_session(&viewer,&login,password,None).await.unwrap();assert_eq!(authenticated,id);assert!(matches!(host_auth.await.unwrap().unwrap(),AuthenticatedTarget::Session{..}));
                }
            }).await.unwrap();
        }
        #[tokio::test]
        async fn capture_failure_keeps_authenticated_control_responsive() {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut state = initial_state(built_in_presets(), "123456".into());
                state.screens[0].settings.bitrate_kbps = 7777;
                let shared = Arc::new(Mutex::new(state));
                let (tx, rx) = mpsc::channel(64);
                let handle = ScreenHandle { tx: tx.clone() };
                let actors = Arc::new(Mutex::new(HashMap::from([(1, handle.clone())])));
                let actor = tokio::spawn(screen_actor(1, shared.clone(), rx, handle));
                let (_h, _c, sender, viewer) = pair().await;
                let (shutdown, watch) = watch::channel(false);
                let peer = tokio::spawn(serve_peer(
                    sender,
                    5,
                    "test-host".into(),
                    shared,
                    actors,
                    watch,
                ));
                bridge_core::security::authenticate_viewer_screen(&viewer, "123456", 1)
                    .await
                    .unwrap();
                let (mut requests, mut replies) = viewer.open_bi().await.unwrap();
                send_control(
                    &mut requests,
                    &ClientMessage::Hello(bridge_core::protocol::ClientHello {
                        version: VERSION,
                        codecs: mac::available_codecs(),
                        initial_settings: None,
                    }),
                )
                .await
                .unwrap();
                assert!(matches!(
                    receive_json::<_, HostMessage>(&mut replies).await.unwrap(),
                    HostMessage::SessionAccepted { .. }
                ));
                loop {
                    if let HostMessage::Failure(f) =
                        receive_json::<_, HostMessage>(&mut replies).await.unwrap()
                    {
                        assert_eq!(f.code, FailureCode::PermissionDenied);
                        break;
                    }
                }
                send_control(&mut requests, &ClientMessage::ClockProbe { t1: 123 })
                    .await
                    .unwrap();
                loop {
                    if let HostMessage::ClockReply { t1, .. } =
                        receive_json::<_, HostMessage>(&mut replies).await.unwrap()
                    {
                        assert_eq!(t1, 123);
                        break;
                    }
                }
                send_control(&mut requests, &ClientMessage::Stop)
                    .await
                    .unwrap();
                peer.await.unwrap().unwrap();
                shutdown.send_replace(true);
                tx.send(ScreenCommand::Remove).await.unwrap();
                actor.await.unwrap();
            })
            .await
            .unwrap();
        }
        #[tokio::test]
        async fn shared_source_restart_failure_detach_and_removal_are_isolated() {
            tokio::time::timeout(Duration::from_secs(15),async{
                let shared=Arc::new(Mutex::new(initial_state(built_in_presets(),"123456".into())));
                let(tx,rx)=mpsc::channel(64);let actor=tokio::spawn(screen_actor(1,shared.clone(),rx,ScreenHandle{tx:tx.clone()}));
                let(_h1,_c1,s1,v1)=pair().await;let(_h2,_c2,s2,_v2)=pair().await;
                let(c1,mut r1)=mpsc::channel(64);let(c2,mut r2)=mpsc::channel(64);
                for(id,connection,controls)in[(11,s1,c1),(22,s2,c2)]{
                    let(reply,accepted)=oneshot::channel();tx.send(ScreenCommand::Join{id,connection,codecs:mac::available_codecs(),controls,reply}).await.unwrap();assert_eq!(accepted.await.unwrap().unwrap().screen_id,1);
                }
                next_matching(&mut r1,|m|matches!(m,HostMessage::StreamStatus{state:MediaState::Ready,..})).await;
                assert_eq!(shared.lock().unwrap().screens[0].viewers.len(),2);
                let generation=shared.lock().unwrap().screens[0].generation;
                let mut settings=shared.lock().unwrap().screens[0].settings.clone();settings.width=1280;settings.height=720;
                tx.send(ScreenCommand::Settings{peer:Some(11),request:Some(1),settings:settings.clone(),revision:Some(4)}).await.unwrap();
                for rx in [&mut r1,&mut r2]{let m=next_matching(rx,|m|matches!(m,HostMessage::ScreenSettingsApplied{screen,..}if screen.revision==4)).await;let HostMessage::ScreenSettingsApplied{screen,..}=m else{unreachable!()};assert!(screen.generation>generation);assert_eq!(screen.settings.width,1280);}
                tx.send(ScreenCommand::Diagnostics{peer:Some(22),enabled:true}).await.unwrap();
                next_matching(&mut r2,|m|matches!(m,HostMessage::ScreenSettingsApplied{screen,..}if screen.instrumented)).await;
                settings.bitrate_kbps=7777;
                tx.send(ScreenCommand::Settings{peer:Some(11),request:Some(2),settings,revision:Some(5)}).await.unwrap();
                for rx in [&mut r1,&mut r2]{next_matching(rx,|m|matches!(m,HostMessage::Failure(f)if f.code==FailureCode::PermissionDenied)).await;}
                tx.send(ScreenCommand::Leave(11)).await.unwrap();v1.close(0u32.into(),b"test leave");
                tx.send(ScreenCommand::Burst(Some(22))).await.unwrap();
                tx.send(ScreenCommand::Remove).await.unwrap();
                next_matching(&mut r2,|m|matches!(m,HostMessage::ScreenRemoved{screen_id:1,..})).await;
                actor.await.unwrap();assert!(shared.lock().unwrap().screens.is_empty());
            }).await.unwrap();
        }
    }
}
