use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::smallvec;
use std::{
    collections::VecDeque,
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

pub struct AudioPlayer {
    _stream: cpal::Stream,
    queue: Arc<Mutex<VecDeque<f32>>>,
    output_rate: u32,
    output_channels: usize,
}

impl AudioPlayer {
    pub fn new() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("No audio output device")?;
        let supported = device.default_output_config()?;
        let output_rate = supported.sample_rate();
        let output_channels = supported.channels() as usize;
        let format = supported.sample_format();
        let config = supported.config();
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let error = |err| eprintln!("AirBridge audio: {err}");
        let stream = match format {
            cpal::SampleFormat::F32 => {
                let queue = queue.clone();
                device.build_output_stream(
                    config.clone(),
                    move |out: &mut [f32], _| fill_audio(out, &queue, |v| v),
                    error,
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let queue = queue.clone();
                device.build_output_stream(
                    config.clone(),
                    move |out: &mut [i16], _| {
                        fill_audio(out, &queue, |v| {
                            (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                        })
                    },
                    error,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                let queue = queue.clone();
                device.build_output_stream(
                    config,
                    move |out: &mut [u16], _| {
                        fill_audio(out, &queue, |v| {
                            ((v.clamp(-1.0, 1.0) + 1.0) * (u16::MAX as f32 / 2.0)) as u16
                        })
                    },
                    error,
                    None,
                )?
            }
            _ => bail!("The audio device uses an unsupported sample format"),
        };
        stream.play()?;
        Ok(Self {
            _stream: stream,
            queue,
            output_rate,
            output_channels,
        })
    }

    pub fn submit(&self, bytes: &[u8], input_rate: u32, input_channels: u8) {
        if input_rate == 0 || input_channels == 0 {
            return;
        }
        let samples = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect::<Vec<_>>();
        let in_channels = input_channels as usize;
        let frames = samples.len() / in_channels;
        if frames < 2 {
            return;
        }
        let ratio = input_rate as f64 / self.output_rate as f64;
        let mut position = 0.0;
        if let Ok(mut queue) = self.queue.lock() {
            while position < (frames - 1) as f64 {
                let index = position.floor() as usize;
                let blend = (position - index as f64) as f32;
                for channel in 0..self.output_channels {
                    let source_channel = channel.min(in_channels - 1);
                    let a = samples[index * in_channels + source_channel];
                    let b = samples[(index + 1) * in_channels + source_channel];
                    queue.push_back(a + (b - a) * blend);
                }
                position += ratio;
            }
            let max_samples = (self.output_rate as usize * self.output_channels) / 10;
            while queue.len() > max_samples {
                queue.pop_front();
            }
        }
    }
}

fn fill_audio<T: Copy>(
    out: &mut [T],
    queue: &Arc<Mutex<VecDeque<f32>>>,
    convert: impl Fn(f32) -> T,
) {
    if let Ok(mut queue) = queue.try_lock() {
        for sample in out {
            *sample = convert(queue.pop_front().unwrap_or(0.0));
        }
    } else {
        for sample in out {
            *sample = convert(0.0);
        }
    }
}
