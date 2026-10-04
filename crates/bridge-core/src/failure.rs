//! Typed failure scope and generation-aware lifecycle transitions.
use crate::protocol::{RequestId, ScreenId, StreamGeneration};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    PermissionDenied,
    ScreenUnavailable,
    Authentication,
    VersionMismatch,
    ApiUnavailable,
    CaptureCreate,
    CaptureStart,
    FirstFrameTimeout,
    Encoder,
    Decoder,
    Graphics,
    WorkerExited,
    WorkerTimeout,
    Protocol,
    Transport,
    Cleanup,
    Rejected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    Request,
    Video,
    Session,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Failure {
    pub code: FailureCode,
    pub scope: FailureScope,
    pub screen_id: ScreenId,
    pub generation: StreamGeneration,
    pub request_id: Option<RequestId>,
    pub retryable: bool,
    pub origin: String,
    pub message: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Starting,
    Active,
    Restarting,
    Stopping,
    Failed,
    Stopped,
}
#[derive(Debug)]
pub struct Lifecycle {
    pub generation: u64,
    pub phase: Phase,
    pub reason: Option<String>,
}
impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            generation: 0,
            phase: Phase::Idle,
            reason: None,
        }
    }
}
impl Lifecycle {
    pub fn start(&mut self, generation: u64) -> bool {
        if generation <= self.generation || self.phase == Phase::Stopping {
            return false;
        }
        self.generation = generation;
        self.phase = Phase::Starting;
        self.reason = None;
        true
    }
    pub fn ready(&mut self, generation: u64) -> bool {
        if generation != self.generation
            || !matches!(self.phase, Phase::Starting | Phase::Restarting)
        {
            return false;
        }
        self.phase = Phase::Active;
        true
    }
    pub fn stop(&mut self, generation: u64, reason: String) -> bool {
        if generation != self.generation {
            return false;
        }
        if self.reason.is_none() || self.reason.as_deref() == Some("Video lane closed") {
            self.reason = Some(reason);
        }
        if matches!(self.phase, Phase::Stopping | Phase::Stopped | Phase::Failed) {
            return false;
        }
        self.phase = Phase::Stopping;
        true
    }
    pub fn stopped(&mut self, generation: u64, failed: bool) -> bool {
        if generation != self.generation || self.phase != Phase::Stopping {
            return false;
        }
        self.phase = if failed {
            Phase::Failed
        } else {
            Phase::Stopped
        };
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_and_duplicate_events_do_not_restart_cleanup() {
        let mut l = Lifecycle::default();
        assert!(l.start(1));
        assert!(l.ready(1));
        assert!(!l.stop(0, "stale".into()));
        assert!(l.stop(1, "Video lane closed".into()));
        assert!(!l.stop(1, "Permission denied".into()));
        assert_eq!(l.reason.as_deref(), Some("Permission denied"));
        assert!(!l.start(2));
        assert!(l.stopped(1, true));
        assert!(l.start(2));
        assert!(!l.stopped(1, false));
    }
}
