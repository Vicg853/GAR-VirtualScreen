//! Bounded, asynchronous stream diagnostics. Media producers only use try_send.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::Instant,
};

pub const FRAME_LIMIT: usize = 20;
pub const HISTORY_LIMIT: usize = 60;
pub const STAGE_COUNT: usize = 23;
pub const EVENT_LIMIT: usize = 1024;
pub fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Capture,
    CaptureQueue,
    Encode,
    SendQueue,
    Write,
    Transfer,
    Receive,
    IpcQueue,
    DecodeQueue,
    Decode,
    RenderQueue,
    Upload,
    Submit,
    GpuComplete,
    PresentSubmit,
    Presentation,
    CpuParse,
    GpuInput,
    DecodeSubmit,
    SurfaceAcquire,
    GpuRender,
    SoftwareDecode,
    SoftwareUpload,
}
impl Stage {
    pub const ALL: [Self; STAGE_COUNT] = [
        Self::Capture,
        Self::CaptureQueue,
        Self::Encode,
        Self::SendQueue,
        Self::Write,
        Self::Transfer,
        Self::Receive,
        Self::IpcQueue,
        Self::DecodeQueue,
        Self::Decode,
        Self::RenderQueue,
        Self::Upload,
        Self::Submit,
        Self::GpuComplete,
        Self::PresentSubmit,
        Self::Presentation,
        Self::CpuParse,
        Self::GpuInput,
        Self::DecodeSubmit,
        Self::SurfaceAcquire,
        Self::GpuRender,
        Self::SoftwareDecode,
        Self::SoftwareUpload,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Capture => "Capture callback",
            Self::CaptureQueue => "Capture queue",
            Self::Encode => "Encode",
            Self::SendQueue => "Sender queue",
            Self::Write => "QUIC write accepted",
            Self::Transfer => "Transfer + reception",
            Self::Receive => "Complete reception",
            Self::IpcQueue => "Native worker IPC queue",
            Self::DecodeQueue => "Decoder queue",
            Self::Decode => "Decode",
            Self::RenderQueue => "Decoded frame queue",
            Self::Upload => "GPU view preparation",
            Self::Submit => "GPU submission",
            Self::GpuComplete => "GPU work completion",
            Self::PresentSubmit => "Presentation submission",
            Self::Presentation => "Monitor presentation",
            Self::CpuParse => "CPU codec parsing",
            Self::GpuInput => "GPU bitstream upload",
            Self::DecodeSubmit => "GPU decode/copy submission",
            Self::SurfaceAcquire => "Swapchain acquisition",
            Self::GpuRender => "GPU render duration",
            Self::SoftwareDecode => "Software decode work",
            Self::SoftwareUpload => "Decoded plane upload submission",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Complete,
    Pending,
    Dropped,
    Failed,
    Cancelled,
    TimedOut,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub start_us: i64,
    pub duration_us: Option<u64>,
    pub outcome: Outcome,
    pub reason: String,
    pub estimated: bool,
}
impl Sample {
    pub fn cell(&self, capture: u64) -> String {
        if self.outcome == Outcome::Unavailable {
            return format!("Unavailable: {}", self.reason);
        }
        let start = (self.start_us.saturating_sub(capture as i64)) as f64 / 1000.;
        let prefix = if self.estimated { "≈" } else { "" };
        let duration = self
            .duration_us
            .map(|d| format!(" D: {:.2}ms", d as f64 / 1000.))
            .unwrap_or_default();
        format!(
            "S: {prefix}{start:+.2}ms{duration} {:?}{}",
            self.outcome,
            if self.reason.is_empty() {
                String::new()
            } else {
                format!(": {}", self.reason)
            }
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameRow {
    pub sequence: u64,
    pub capture_us: u64,
    pub uncertainty_us: Option<u64>,
    pub stages: [Option<Sample>; STAGE_COUNT],
}
impl FrameRow {
    pub fn summary(&self) -> String {
        let last = Stage::ALL
            .into_iter()
            .filter_map(|stage| {
                self.stages[stage as usize]
                    .as_ref()
                    .filter(|s| s.outcome != Outcome::Pending && s.outcome != Outcome::Unavailable)
                    .map(|s| (stage, s))
            })
            .max_by_key(|(_, s)| s.start_us.saturating_add(s.duration_us.unwrap_or(0) as i64));
        let failures = Stage::ALL
            .into_iter()
            .filter_map(|stage| {
                self.stages[stage as usize]
                    .as_ref()
                    .filter(|s| {
                        matches!(
                            s.outcome,
                            Outcome::Failed
                                | Outcome::Dropped
                                | Outcome::Cancelled
                                | Outcome::TimedOut
                        )
                    })
                    .map(|s| format!("{} {:?}: {}", stage.label(), s.outcome, s.reason))
            })
            .collect::<Vec<_>>();
        let failure = if failures.is_empty() {
            "No reported failure; missing stages are not observed".into()
        } else {
            failures.join("; ")
        };
        match last {
            Some((stage, s)) => {
                let elapsed = if self.uncertainty_us == Some(u64::MAX) {
                    "Elapsed unavailable until clock synchronization; local durations valid".into()
                } else {
                    format!(
                        "Observed {:.2}ms",
                        s.start_us
                            .saturating_add(s.duration_us.unwrap_or(0) as i64)
                            .saturating_sub(self.capture_us as i64) as f64
                            / 1000.
                    )
                };
                format!("{elapsed} through {} · {failure}", stage.label())
            }
            None => format!("No concluded stage observations · {failure}"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Average {
    pub count: u64,
    pub mean_start_us: f64,
    pub start_count: u64,
    pub mean_duration_us: f64,
    pub duration_count: u64,
    pub failures: u64,
    pub incomplete: u64,
    pub last_result: String,
}
impl Average {
    fn add(&mut self, sample: &Sample, capture: u64, synchronized: bool) {
        if sample.outcome == Outcome::Pending {
            self.last_result = "Pending".into();
            return;
        }
        self.count += 1;
        if sample.outcome == Outcome::Complete && synchronized {
            self.start_count += 1;
            self.mean_start_us += ((sample.start_us.saturating_sub(capture as i64)) as f64
                - self.mean_start_us)
                / self.start_count as f64;
        }
        if let Some(d) = sample
            .duration_us
            .filter(|_| sample.outcome == Outcome::Complete)
        {
            self.duration_count += 1;
            self.mean_duration_us +=
                (d as f64 - self.mean_duration_us) / self.duration_count as f64;
        }
        if matches!(
            sample.outcome,
            Outcome::Failed | Outcome::Dropped | Outcome::Cancelled | Outcome::TimedOut
        ) {
            self.failures += 1;
        }
        if sample.outcome != Outcome::Complete {
            self.incomplete += 1;
        }
        self.last_result = format!("{:?} {}", sample.outcome, sample.reason);
    }
}
/// Reserved schema only: no audio capture or audio counters are activated.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioStats {
    pub bitrate_kbps: f64,
    pub sample_rate: u32,
    pub channels: u8,
    pub packets_per_second: f64,
    pub underruns: u64,
    pub jitter_buffer_ms: f64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StreamStats {
    pub at_us: u64,
    pub bitrate_kbps: f64,
    pub fps: f64,
    pub decoded_fps: Option<f64>,
    pub presented_fps: Option<f64>,
    pub gpu_memory_bytes: Option<u64>,
    pub gpu_in_flight: Option<u32>,
    pub frame_age_us: Option<u64>,
    pub engine: Option<String>,
    pub frames: u64,
    pub dropped: u64,
    pub rtt_us: u64,
    pub jitter_us: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub capture_queue: Option<u32>,
    pub send_queue: Option<u32>,
    pub decode_queue: Option<u32>,
    pub audio_bitrate_kbps: Option<f64>,
    pub audio_fps: Option<f64>,
    pub audio: Option<AudioStats>,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub frames: Vec<FrameRow>,
    pub averages: [Average; STAGE_COUNT],
    pub history: Vec<StreamStats>,
    pub lost_events: u64,
    pub peer_stats: Option<StreamStats>,
    pub peer_lost_events: u64,
    pub notice: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceEvent {
    pub sequence: u64,
    pub capture_us: u64,
    pub uncertainty_us: Option<u64>,
    pub stage: Stage,
    pub sample: Sample,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticBatch {
    pub screen_id: u32,
    pub generation: u64,
    pub events: Vec<TraceEvent>,
    pub stats: Option<StreamStats>,
    pub lost_events: u64,
}

enum Event {
    Trace(TraceEvent, bool),
    Stats(StreamStats),
    PeerStats(StreamStats),
    Notice(String),
}
struct Store {
    frames: VecDeque<FrameRow>,
    averages: [Average; STAGE_COUNT],
    history: VecDeque<StreamStats>,
    outbound: VecDeque<TraceEvent>,
    last_stats: Option<StreamStats>,
    peer_stats: Option<StreamStats>,
    notice: Option<String>,
}

pub struct Telemetry {
    tx: mpsc::SyncSender<Event>,
    store: Arc<Mutex<Store>>,
    lost: Arc<AtomicU64>,
    burst: AtomicU32,
    next_sample: AtomicU64,
    peer_lost: AtomicU64,
}
impl Telemetry {
    pub fn new() -> Arc<Self> {
        let store = Arc::new(Mutex::new(Store {
            frames: VecDeque::new(),
            averages: std::array::from_fn(|_| Average::default()),
            history: VecDeque::new(),
            outbound: VecDeque::new(),
            last_stats: None,
            peer_stats: None,
            notice: None,
        }));
        let (tx, rx) = mpsc::sync_channel(EVENT_LIMIT);
        let worker = store.clone();
        let lost = Arc::new(AtomicU64::new(0));
        let lost_worker = lost.clone();
        std::thread::Builder::new()
            .name("airbridge-diagnostics".into())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    let Ok(mut store) = worker.lock() else {
                        break;
                    };
                    match event {
                        Event::Notice(message) => {
                            store.notice = Some(message);
                        }
                        Event::PeerStats(stats) => {
                            store.peer_stats = Some(stats);
                        }
                        Event::Stats(stats) => {
                            store.last_stats = Some(stats.clone());
                            store.history.push_back(stats);
                            if store.history.len() > HISTORY_LIMIT {
                                store.history.pop_front();
                            }
                        }
                        Event::Trace(event, export) => {
                            if store.frames.len() == FRAME_LIMIT
                                && store
                                    .frames
                                    .front()
                                    .is_some_and(|f| event.sequence < f.sequence)
                            {
                                continue;
                            }
                            let pos = store
                                .frames
                                .iter()
                                .position(|f| f.sequence == event.sequence);
                            let pos = pos.unwrap_or_else(|| {
                                if store.frames.len() == FRAME_LIMIT {
                                    store.frames.pop_front();
                                }
                                store.frames.push_back(FrameRow {
                                    sequence: event.sequence,
                                    capture_us: event.capture_us,
                                    uncertainty_us: event.uncertainty_us,
                                    stages: std::array::from_fn(|_| None),
                                });
                                store.frames.make_contiguous().sort_by_key(|f| f.sequence);
                                store
                                    .frames
                                    .iter()
                                    .position(|f| f.sequence == event.sequence)
                                    .unwrap()
                            });
                            if event.uncertainty_us.is_some() {
                                store.frames[pos].uncertainty_us = event.uncertainty_us;
                            }
                            let index = event.stage as usize;
                            // A late Pending event must not replace a completed stage.
                            let previous = store.frames[pos].stages[index].as_ref();
                            if previous.is_some_and(|s| s.outcome != Outcome::Pending) {
                                continue;
                            }
                            if export {
                                if store.outbound.len() == EVENT_LIMIT {
                                    store.outbound.pop_front();
                                    lost_worker.fetch_add(1, Ordering::Relaxed);
                                }
                                store.outbound.push_back(event.clone());
                            }
                            store.averages[index].add(
                                &event.sample,
                                event.capture_us,
                                !event.sample.estimated || event.uncertainty_us != Some(u64::MAX),
                            );
                            store.frames[pos].stages[index] = Some(event.sample);
                        }
                    }
                }
            })
            .ok();
        Arc::new(Self {
            tx,
            store,
            lost,
            burst: AtomicU32::new(0),
            next_sample: AtomicU64::new(0),
            peer_lost: AtomicU64::new(0),
        })
    }
    pub fn trace(&self, event: TraceEvent) {
        if self.tx.try_send(Event::Trace(event, true)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn ingest(&self, event: TraceEvent) {
        if self.tx.try_send(Event::Trace(event, false)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn delivery_failed(&self, count: u64, message: &str) {
        self.lost.fetch_add(count, Ordering::Relaxed);
        let _ = self
            .tx
            .try_send(Event::Notice(message.chars().take(160).collect()));
    }
    pub fn peer_stats(&self, stats: StreamStats) {
        if self.tx.try_send(Event::PeerStats(stats)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn peer_lost(&self, lost: u64) {
        self.peer_lost.store(lost, Ordering::Relaxed);
    }
    pub fn stats(&self, stats: StreamStats) {
        if self.tx.try_send(Event::Stats(stats)).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn sample(&self, now: u64) -> bool {
        if self
            .burst
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1))
            .is_ok()
        {
            self.next_sample
                .store(now.saturating_add(1_000_000), Ordering::Relaxed);
            return true;
        }
        let next = self.next_sample.load(Ordering::Relaxed);
        now >= next
            && self
                .next_sample
                .compare_exchange(
                    next,
                    now.saturating_add(1_000_000),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
    }
    pub fn burst(&self) {
        self.burst.store(FRAME_LIMIT as u32, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> Snapshot {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        Snapshot {
            frames: store.frames.iter().cloned().collect(),
            averages: store.averages.clone(),
            history: store.history.iter().cloned().collect(),
            lost_events: self.lost.load(Ordering::Relaxed),
            peer_stats: store.peer_stats.clone(),
            peer_lost_events: self.peer_lost.load(Ordering::Relaxed),
            notice: store.notice.clone(),
        }
    }
    pub fn drain(&self, screen_id: u32, generation: u64) -> DiagnosticBatch {
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        DiagnosticBatch {
            screen_id,
            generation,
            events: store.outbound.drain(..).collect(),
            stats: store.last_stats.take(),
            lost_events: self.lost.load(Ordering::Relaxed),
        }
    }
    pub fn finish(&self, now_host_us: i64, reason: &str) {
        for f in self.snapshot().frames {
            for stage in Stage::ALL {
                if let Some(s) = &f.stages[stage as usize] {
                    if s.outcome == Outcome::Pending {
                        self.trace(TraceEvent {
                            sequence: f.sequence,
                            capture_us: f.capture_us,
                            uncertainty_us: f.uncertainty_us,
                            stage,
                            sample: Sample {
                                start_us: s.start_us,
                                duration_us: Some((now_host_us - s.start_us).max(0) as u64),
                                outcome: Outcome::Cancelled,
                                reason: reason.chars().take(160).collect(),
                                estimated: s.estimated,
                            },
                        });
                    }
                }
            }
        }
    }
    pub fn expire(&self, now_host_us: i64, reason: &str) {
        let pending =
            self.snapshot()
                .frames
                .into_iter()
                .flat_map(|f| {
                    Stage::ALL.into_iter().filter_map(move |stage| {
                        let s = f.stages[stage as usize].as_ref()?;
                        (s.outcome == Outcome::Pending && now_host_us - s.start_us >= 5_000_000)
                            .then(|| TraceEvent {
                                sequence: f.sequence,
                                capture_us: f.capture_us,
                                uncertainty_us: f.uncertainty_us,
                                stage,
                                sample: Sample {
                                    start_us: s.start_us,
                                    duration_us: Some((now_host_us - s.start_us).max(0) as u64),
                                    outcome: Outcome::TimedOut,
                                    reason: reason.into(),
                                    estimated: s.estimated,
                                },
                            })
                    })
                })
                .collect::<Vec<_>>();
        for e in pending {
            self.trace(e);
        }
    }
}

/// A token exists only in the compiled instrumented path, and only for sampled frames.
#[derive(Clone)]
pub struct TraceToken {
    pub sequence: u64,
    pub capture_us: u64,
    pub offset_us: i64,
    pub uncertainty_us: Option<u64>,
    pub telemetry: Arc<Telemetry>,
}
impl std::fmt::Debug for TraceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceToken")
            .field("sequence", &self.sequence)
            .finish()
    }
}
impl TraceToken {
    pub fn mark(&self, stage: Stage, start: u64, end: Option<u64>, outcome: Outcome, reason: &str) {
        self.telemetry.trace(TraceEvent {
            sequence: self.sequence,
            capture_us: self.capture_us,
            uncertainty_us: self.uncertainty_us,
            stage,
            sample: Sample {
                start_us: (start as i64).saturating_add(self.offset_us),
                duration_us: end.map(|e| e.saturating_sub(start)),
                outcome,
                reason: reason.chars().take(160).collect(),
                estimated: self.uncertainty_us.is_some(),
            },
        });
    }
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ClockEstimate {
    pub offset_us: i64,
    pub rtt_us: u64,
    pub uncertainty_us: u64,
    pub measured_at_us: u64,
}
impl ClockEstimate {
    pub fn from_probe(t1: u64, t2: u64, t3: u64, t4: u64) -> Option<Self> {
        if t4 < t1 || t3 < t2 {
            return None;
        }
        let rtt = (t4 - t1).checked_sub(t3 - t2)?;
        if rtt > 5_000_000 {
            return None;
        }
        let offset = ((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2;
        Some(Self {
            offset_us: i64::try_from(offset).ok()?,
            rtt_us: rtt,
            uncertainty_us: rtt.div_ceil(2),
            measured_at_us: t4,
        })
    }
    pub fn update(&mut self, sample: Self) {
        if self.measured_at_us == 0
            || sample.rtt_us <= self.rtt_us
            || sample.measured_at_us.saturating_sub(self.measured_at_us) > 30_000_000
        {
            *self = sample;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimingView {
    #[default]
    Frames,
    Averages,
}
#[derive(Clone, Default)]
pub struct StatsView {
    pub open: bool,
    pub mode: TimingView,
    pub frozen: Option<Snapshot>,
    pub freeze_after: Option<u64>,
    cached: Option<Snapshot>,
    last_refresh: u64,
}
impl StatsView {
    pub fn reset(&mut self) {
        self.frozen = None;
        self.cached = None;
        self.freeze_after = None;
        self.last_refresh = 0;
    }

    pub fn refresh(&mut self, telemetry: &Telemetry) {
        let now = now_us();
        if self.cached.is_none() || now.saturating_sub(self.last_refresh) >= 500_000 {
            let snapshot = telemetry.snapshot();
            if self.freeze_after.is_some_and(|n| {
                snapshot.averages[Stage::Capture as usize].count >= n
                    && snapshot.frames.iter().all(|f| {
                        f.stages
                            .iter()
                            .flatten()
                            .all(|s| s.outcome != Outcome::Pending)
                    })
            }) {
                self.frozen = Some(snapshot.clone());
                self.freeze_after = None;
            }
            self.cached = Some(snapshot);
            self.last_refresh = now;
        }
    }

    pub fn lines(&self, telemetry: &Telemetry) -> Vec<String> {
        let snapshot = self
            .frozen
            .clone()
            .or_else(|| self.cached.clone())
            .unwrap_or_else(|| telemetry.snapshot());
        let mut lines = vec![format!(
            "Diagnostics lost: {} · sampled frames · clock-translated times are estimates",
            snapshot.lost_events
        )];
        if let Some(notice) = &snapshot.notice {
            lines.push(notice.clone());
        }
        let queue = |v: Option<u32>| {
            v.map(|v| v.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        let fps = |v: Option<f64>| {
            v.map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "unavailable".into())
        };
        if let Some(s) = &snapshot.peer_stats {
            lines.push(format!("Peer: {:.1} FPS · decoded {} FPS · drops {} · capture queue {} · sender queue {} · decoder queue {} · diagnostic loss {}",s.fps,fps(s.decoded_fps),s.dropped,queue(s.capture_queue),queue(s.send_queue),queue(s.decode_queue),snapshot.peer_lost_events));
        }
        if let Some(s) = snapshot.history.last() {
            lines.push(format!("{:.0} kbps · {:.1} FPS · RTT {:.2}ms · RTT variation {:.2}ms · drops {} · connection packet loss {} · congestion events {}",s.bitrate_kbps,s.fps,s.rtt_us as f64/1000.,s.jitter_us as f64/1000.,s.dropped,s.lost_packets,s.congestion_events));
            lines.push(format!("Decode FPS {} · capture queue {} · sender queue {} · decoder queue {} · audio: unavailable",fps(s.decoded_fps),queue(s.capture_queue),queue(s.send_queue),queue(s.decode_queue)));
            if let Some(engine) = &s.engine {
                lines.push(format!("{engine} · presentation submit {} FPS · GPU in flight {} · accounted video memory {:.1} MiB (excludes driver/swapchain) · frame age {}",
                    fps(s.presented_fps), queue(s.gpu_in_flight), s.gpu_memory_bytes.unwrap_or(0) as f64 / 1048576.,
                    s.frame_age_us.map(|age| format!("≈{:.2}ms to compositor submission", age as f64 / 1000.)).unwrap_or_else(|| "unavailable".into())));
            }
            // Compact history chart; never retain frame buffers in statistics.
            let chart = |values: Vec<f64>| {
                let max = values.iter().copied().fold(1f64, f64::max);
                let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
                values
                    .into_iter()
                    .map(|v| bars[((v / max * 7.).round() as usize).min(7)])
                    .collect::<String>()
            };
            lines.push(format!(
                "Bitrate {}",
                chart(snapshot.history.iter().map(|s| s.bitrate_kbps).collect())
            ));
            lines.push(format!(
                "FPS     {}",
                chart(snapshot.history.iter().map(|s| s.fps).collect())
            ));
            lines.push(format!(
                "RTT     {}",
                chart(snapshot.history.iter().map(|s| s.rtt_us as f64).collect())
            ));
        }
        lines
    }
    pub fn table(&self, telemetry: &Telemetry) -> Vec<Vec<String>> {
        let snapshot = self
            .frozen
            .clone()
            .or_else(|| self.cached.clone())
            .unwrap_or_else(|| telemetry.snapshot());
        match self.mode {
            TimingView::Frames => {
                let mut rows = vec![
                    std::iter::once("Frame".into())
                        .chain(Stage::ALL.into_iter().map(|s| s.label().into()))
                        .chain(std::iter::once("Summary".into()))
                        .collect(),
                ];
                rows.extend(snapshot.frames.iter().map(|f| {
                    std::iter::once(format!(
                        "{} · {}",
                        f.sequence,
                        match f.uncertainty_us {
                            Some(u64::MAX) => "clock pending".into(),
                            Some(u) => format!("±{u}µs"),
                            None => "host clock".into(),
                        }
                    ))
                    .chain(f.stages.iter().map(|s| {
                        s.as_ref()
                            .map(|s| {
                                if s.estimated && f.uncertainty_us == Some(u64::MAX) {
                                    format!("{:?} · clock not synchronized", s.outcome)
                                } else {
                                    s.cell(f.capture_us)
                                }
                            })
                            .unwrap_or_else(|| {
                                if f.stages.iter().flatten().any(|s| {
                                    matches!(
                                        s.outcome,
                                        Outcome::Failed
                                            | Outcome::Dropped
                                            | Outcome::Cancelled
                                            | Outcome::TimedOut
                                    )
                                }) {
                                    "Not reached / not observed".into()
                                } else {
                                    "Not observed yet".into()
                                }
                            })
                    }))
                    .chain(std::iter::once(f.summary()))
                    .collect()
                }));
                rows
            }
            TimingView::Averages => {
                let mut rows = vec![vec![
                    "Stage".into(),
                    "Start after capture (avg)".into(),
                    "Stage duration (avg)".into(),
                    "Samples".into(),
                    "Failures / incomplete".into(),
                    "Last result".into(),
                ]];
                rows.extend(
                    Stage::ALL
                        .into_iter()
                        .zip(&snapshot.averages)
                        .map(|(stage, a)| {
                            vec![
                                stage.label().into(),
                                if a.start_count == 0 {
                                    "Unavailable".into()
                                } else {
                                    format!("{:+.2}ms", a.mean_start_us / 1000.)
                                },
                                if a.duration_count == 0 {
                                    "Unavailable".into()
                                } else {
                                    format!("{:.2}ms", a.mean_duration_us / 1000.)
                                },
                                a.count.to_string(),
                                format!("{} / {}", a.failures, a.incomplete),
                                a.last_result.clone(),
                            ]
                        }),
                );
                rows
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait_for(t: &Telemetry, predicate: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !predicate(&t.snapshot()) {
            assert!(
                Instant::now() < deadline,
                "diagnostic worker did not process test data"
            );
            std::thread::yield_now();
        }
    }
    fn event(sequence: u64, outcome: Outcome) -> TraceEvent {
        TraceEvent {
            sequence,
            capture_us: 100,
            uncertainty_us: None,
            stage: Stage::Encode,
            sample: Sample {
                start_us: 120,
                duration_us: if outcome == Outcome::Pending {
                    None
                } else {
                    Some(10)
                },
                outcome,
                reason: "test".into(),
                estimated: false,
            },
        }
    }
    #[test]
    fn late_pending_and_duplicates_do_not_corrupt_averages() {
        let t = Telemetry::new();
        t.trace(event(1, Outcome::Complete));
        t.trace(event(1, Outcome::Pending));
        t.trace(event(1, Outcome::Complete));
        wait_for(&t, |s| s.averages[Stage::Encode as usize].count == 1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let s = t.snapshot();
        assert_eq!(s.averages[Stage::Encode as usize].count, 1);
        assert_eq!(
            s.frames[0].stages[Stage::Encode as usize]
                .as_ref()
                .unwrap()
                .outcome,
            Outcome::Complete
        );
    }
    #[test]
    fn timeout_and_cancellation_keep_elapsed_and_failure_data() {
        let t = Telemetry::new();
        t.trace(event(1, Outcome::Pending));
        wait_for(&t, |s| !s.frames.is_empty());
        t.expire(5_000_200, "timeout");
        wait_for(&t, |s| s.averages[Stage::Encode as usize].failures == 1);
        let s = t.snapshot();
        let sample = s.frames[0].stages[Stage::Encode as usize].as_ref().unwrap();
        assert_eq!(sample.outcome, Outcome::TimedOut);
        assert_eq!(sample.duration_us, Some(5_000_080));
        assert!(s.frames[0].summary().contains("TimedOut"));
        t.trace(event(2, Outcome::Pending));
        wait_for(&t, |s| s.frames.len() == 2);
        t.finish(250, "disconnected");
        wait_for(&t, |s| s.averages[Stage::Encode as usize].failures == 2);
        assert_eq!(
            t.snapshot().frames[1].stages[Stage::Encode as usize]
                .as_ref()
                .unwrap()
                .duration_us,
            Some(130)
        );
    }
    #[test]
    fn frames_and_stage_averages_are_separate_views_and_freeze_is_stable() {
        let t = Telemetry::new();
        t.trace(event(1, Outcome::Complete));
        wait_for(&t, |s| !s.frames.is_empty());
        let mut view = StatsView::default();
        view.refresh(&t);
        assert_eq!(view.table(&t).len(), 2);
        view.frozen = Some(t.snapshot());
        t.trace(event(2, Outcome::Complete));
        wait_for(&t, |s| s.frames.len() == 2);
        assert_eq!(view.table(&t).len(), 2);
        view.mode = TimingView::Averages;
        assert_eq!(view.table(&t).len(), STAGE_COUNT + 1);
        view.reset();
        view.refresh(&t);
        view.mode = TimingView::Frames;
        assert_eq!(view.table(&t).len(), 3);
    }
    #[test]
    fn clock_filter_refreshes_stale_offsets() {
        let mut c = ClockEstimate {
            offset_us: 2,
            rtt_us: 10,
            uncertainty_us: 5,
            measured_at_us: 100,
        };
        c.update(ClockEstimate {
            offset_us: 9,
            rtt_us: 20,
            uncertainty_us: 10,
            measured_at_us: 200,
        });
        assert_eq!(c.offset_us, 2);
        c.update(ClockEstimate {
            offset_us: 9,
            rtt_us: 20,
            uncertainty_us: 10,
            measured_at_us: 31_000_100,
        });
        assert_eq!(c.offset_us, 9);
    }
    #[test]
    fn clock_math_and_invalid_probes() {
        let c = ClockEstimate::from_probe(100, 126, 128, 114).unwrap();
        assert_eq!(c.offset_us, 20);
        assert_eq!(c.rtt_us, 12);
        assert_eq!(c.uncertainty_us, 6);
        assert!(ClockEstimate::from_probe(100, 126, 128, 101).is_none());
        assert!(ClockEstimate::from_probe(100, 128, 126, 114).is_none());
    }
    #[test]
    fn rolling_average_keeps_no_history() {
        let mut a = Average::default();
        for d in [10, 20, 30] {
            a.add(
                &Sample {
                    start_us: 100,
                    duration_us: Some(d),
                    outcome: Outcome::Complete,
                    reason: String::new(),
                    estimated: false,
                },
                100,
                true,
            );
        }
        assert_eq!(a.mean_duration_us, 20.);
        assert_eq!(a.count, 3);
    }
    #[test]
    fn bounded_store_and_sampling() {
        let t = Telemetry::new();
        assert!(t.sample(0));
        assert!(!t.sample(1));
        t.burst();
        for i in 1..=20 {
            assert!(t.sample(i));
        }
        assert!(!t.sample(21));
        for i in 0..100 {
            t.stats(StreamStats::default());
            t.trace(TraceEvent {
                sequence: i,
                capture_us: 0,
                uncertainty_us: None,
                stage: Stage::Encode,
                sample: Sample {
                    start_us: 0,
                    duration_us: Some(1),
                    outcome: Outcome::Complete,
                    reason: String::new(),
                    estimated: false,
                },
            });
        }
        wait_for(&t, |s| s.averages[Stage::Encode as usize].count == 100);
        let s = t.snapshot();
        assert!(s.frames.len() <= FRAME_LIMIT);
        assert!(s.history.len() <= HISTORY_LIMIT);
        assert_eq!(s.averages[Stage::Encode as usize].count, 100);
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;
    #[test]
    fn failures_and_unavailable_do_not_pollute_success_timings() {
        let mut average = Average::default();
        for outcome in [
            Outcome::Failed,
            Outcome::Dropped,
            Outcome::Cancelled,
            Outcome::TimedOut,
            Outcome::Unavailable,
        ] {
            average.add(
                &Sample {
                    start_us: 9_000_000,
                    duration_us: Some(8_000_000),
                    outcome,
                    reason: "not completed".into(),
                    estimated: false,
                },
                0,
                true,
            );
        }
        assert_eq!(average.start_count, 0);
        assert_eq!(average.duration_count, 0);
        average.add(
            &Sample {
                start_us: 120,
                duration_us: Some(30),
                outcome: Outcome::Complete,
                reason: String::new(),
                estimated: false,
            },
            100,
            true,
        );
        assert_eq!(average.mean_start_us, 20.);
        assert_eq!(average.mean_duration_us, 30.);
        assert_eq!(average.failures, 4);
        assert_eq!(average.incomplete, 5);
    }
    #[test]
    fn unavailable_scanout_has_no_fabricated_start_time() {
        let sample = Sample {
            start_us: 50_760,
            duration_us: None,
            outcome: Outcome::Unavailable,
            reason: "No monitor feedback".into(),
            estimated: true,
        };
        assert_eq!(sample.cell(0), "Unavailable: No monitor feedback");
    }
}
