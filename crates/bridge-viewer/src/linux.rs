use anyhow::{Context, Result};
use bridge_core::diagnostics::Diagnostics;
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::smallvec;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

#[derive(Default)]
pub struct VideoFrame {
    pub image: Option<Arc<RenderImage>>,
    pub frames: u64,
    pub error: Option<String>,
}

pub struct VideoDecoder {
    input: tokio::sync::mpsc::Sender<Vec<u8>>,
    child: Arc<Mutex<Child>>,
}

impl VideoDecoder {
    pub fn new(
        width: u32,
        height: u32,
        target: Arc<Mutex<VideoFrame>>,
        diagnostics: Arc<Diagnostics>,
    ) -> Result<Self> {
        let mut command = Command::new("ffmpeg");
        // `-fflags nobuffer` discards H.264 packets during probing on a pipe.
        // Keep the decoder independent from the host encoder's acceleration choice.
        command.args(["-hide_banner", "-loglevel", "warning", "-f", "h264"]);
        let mut child = command
            .args([
                "-i", "pipe:0", "-an", "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(
                "FFmpeg is required for H.264 decoding; install it from your Linux package manager",
            )?;
        let mut stdin = child.stdin.take().context("Cannot feed the decoder")?;
        let mut stdout = child.stdout.take().context("Cannot read decoded frames")?;
        let stderr = child
            .stderr
            .take()
            .context("Cannot read FFmpeg diagnostics")?;
        let diagnostic_output = diagnostics.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) if !line.is_empty() => diagnostic_output.record("FFmpeg", line),
                    Ok(_) => {}
                    Err(error) => {
                        diagnostic_output.record("FFmpeg", format!("stderr: {error}"));
                        break;
                    }
                }
            }
        });
        let child = Arc::new(Mutex::new(child));
        let (send, mut receive) = tokio::sync::mpsc::channel::<Vec<u8>>(3);
        let target_writer = target.clone();
        let diagnostic_writer = diagnostics.clone();
        thread::spawn(move || {
            while let Some(packet) = receive.blocking_recv() {
                if let Err(error) = stdin.write_all(&packet) {
                    let message = format!("Decoder input stopped: {error}");
                    diagnostic_writer.record("decoder", &message);
                    target_writer.lock().unwrap().error = Some(message);
                    break;
                }
            }
        });
        let diagnostic_reader = diagnostics.clone();
        let target_reader = target.clone();
        thread::spawn(move || {
            let Some(frame_size) = (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
            else {
                diagnostic_reader.record("decoder", "Invalid decoded frame dimensions");
                return;
            };
            while frame_size <= 7680 * 4320 * 4 {
                let mut bytes = vec![0; frame_size];
                if let Err(error) = stdout.read_exact(&mut bytes) {
                    let message = format!("Decoded frame output stopped: {error}");
                    diagnostic_reader.record("decoder", &message);
                    if let Ok(mut target) = target_reader.lock() {
                        target.error = Some(message);
                    }
                    break;
                }
                if let Some(buffer) = RgbaImage::from_raw(width, height, bytes) {
                    let image = Arc::new(RenderImage::new(smallvec![Frame::new(buffer)]));
                    if let Ok(mut target) = target.lock() {
                        target.image = Some(image);
                        target.frames += 1;
                        if target.frames == 1 || target.frames % 120 == 0 {
                            diagnostic_reader
                                .record("decoder", format!("Decoded {} frames", target.frames));
                        }
                    }
                }
            }
        });
        Ok(Self { input: send, child })
    }

    pub async fn submit(&self, packet: Vec<u8>) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), self.input.send(packet))
            .await
            .context("Decoder has not accepted a frame for five seconds")?
            .context("Decoder input has stopped")
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}
