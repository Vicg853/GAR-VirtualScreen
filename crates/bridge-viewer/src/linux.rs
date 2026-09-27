use anyhow::{Context, Result};
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::smallvec;
use std::{
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Default)]
pub struct VideoFrame {
    pub image: Option<Arc<RenderImage>>,
    pub frames: u64,
    pub dropped: u64,
    pub error: Option<String>,
}

pub struct VideoDecoder {
    input: mpsc::SyncSender<Vec<u8>>,
    child: Arc<Mutex<Child>>,
}

impl VideoDecoder {
    pub fn new(
        width: u32,
        height: u32,
        target: Arc<Mutex<VideoFrame>>,
        update: UnboundedSender<()>,
        hardware_preferred: bool,
    ) -> Result<Self> {
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
        ]);
        if hardware_preferred {
            command.args(["-hwaccel", "auto"]);
        }
        let mut child = command
            .args([
                "-f", "h264", "-i", "pipe:0", "-an", "-f", "rawvideo", "-pix_fmt", "bgra", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context(
                "FFmpeg is required for H.264 decoding; install it from your Linux package manager",
            )?;
        let mut stdin = child.stdin.take().context("Cannot feed the decoder")?;
        let mut stdout = child.stdout.take().context("Cannot read decoded frames")?;
        let child = Arc::new(Mutex::new(child));
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(3);
        let target_writer = target.clone();
        thread::spawn(move || {
            while let Ok(packet) = receive.recv() {
                if stdin.write_all(&packet).is_err() {
                    target_writer.lock().unwrap().error = Some("Video decoder stopped".into());
                    break;
                }
            }
        });
        thread::spawn(move || {
            let Some(frame_size) = (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
            else {
                return;
            };
            while frame_size <= 7680 * 4320 * 4 {
                let mut bytes = vec![0; frame_size];
                if stdout.read_exact(&mut bytes).is_err() {
                    break;
                }
                if let Some(buffer) = RgbaImage::from_raw(width, height, bytes) {
                    let image = Arc::new(RenderImage::new(smallvec![Frame::new(buffer)]));
                    if let Ok(mut target) = target.lock() {
                        target.image = Some(image);
                        target.frames += 1;
                    }
                    let _ = update.send(());
                }
            }
        });
        Ok(Self { input: send, child })
    }

    pub fn submit(&self, packet: Vec<u8>) -> Result<()> {
        self.input
            .try_send(packet)
            .map_err(|_| anyhow::anyhow!("Decoder is busy or has stopped"))
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}
