use anyhow::{Result, bail};
use bridge_core::diagnostics::Diagnostics;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub skipped: u64,
    pub decoder: String,
    pub error: Option<String>,
    pub closed: bool,
    pub user_closed: bool,
    pub cleanup_complete: bool,
}

#[derive(Clone)]
pub struct VideoWindow {
    message: String,
}

impl VideoWindow {
    pub fn spawn_mode(
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
        _instrumented: bool,
    ) -> Result<Self> {
        Self::spawn(target, diagnostics)
    }
    pub async fn submit_debug(
        &self,
        _bytes: Vec<u8>,
        _keyframe: bool,
        _sequence: u64,
        _trace: Option<bridge_core::telemetry::TraceToken>,
    ) -> Result<()> {
        bail!("{}", self.message)
    }
    pub fn spawn(target: Arc<Mutex<VideoFrame>>, diagnostics: Arc<Diagnostics>) -> Result<Self> {
        let message = "The native video display backend is currently implemented for Linux only";
        diagnostics.record("video-window", message);
        if let Ok(mut frame) = target.lock() {
            frame.error = Some(message.into());
            frame.closed = true;
            frame.cleanup_complete = true;
        }
        Ok(Self {
            message: message.into(),
        })
    }

    pub fn unavailable(message: String, target: Arc<Mutex<VideoFrame>>) -> Self {
        if let Ok(mut frame) = target.lock() {
            frame.error = Some(message.clone());
            frame.closed = true;
            frame.cleanup_complete = true;
        }
        Self { message }
    }

    pub fn shutdown_backend() {}

    pub fn show(&self) -> Result<()> {
        bail!("{}", self.message)
    }

    pub async fn close_wait(&self) -> Result<()> {
        self.close()
    }
    pub fn close(&self) -> Result<()> {
        Ok(())
    }

    pub async fn reset(&mut self, _width: u32, _height: u32) -> Result<()> {
        bail!("{}", self.message)
    }

    pub async fn submit(&self, _bytes: Vec<u8>, _keyframe: bool) -> Result<()> {
        bail!("{}", self.message)
    }
}
