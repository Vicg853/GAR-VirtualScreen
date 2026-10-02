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
}

#[derive(Clone)]
pub struct VideoWindow {
    message: String,
}

impl VideoWindow {
    pub fn spawn(target: Arc<Mutex<VideoFrame>>, diagnostics: Arc<Diagnostics>) -> Result<Self> {
        let message = "The native video display backend is currently implemented for Linux only";
        diagnostics.record("video-window", message);
        if let Ok(mut frame) = target.lock() {
            frame.error = Some(message.into());
        }
        Ok(Self {
            message: message.into(),
        })
    }

    pub fn unavailable(message: String, _target: Arc<Mutex<VideoFrame>>) -> Self {
        Self { message }
    }

    pub fn show(&self) -> Result<()> {
        bail!("{}", self.message)
    }

    pub fn close(&self) -> Result<()> {
        Ok(())
    }

    pub async fn reset(&self, _width: u32, _height: u32) -> Result<()> {
        bail!("{}", self.message)
    }

    pub async fn submit(&self, _bytes: Vec<u8>, _keyframe: bool) -> Result<()> {
        bail!("{}", self.message)
    }
}
