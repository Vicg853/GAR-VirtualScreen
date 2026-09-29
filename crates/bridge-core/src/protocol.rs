use crate::config::{CodecOption, VideoSettings};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u16 = 2;
pub const PORT: u16 = 44990;
pub const MAX_CONTROL: usize = 64 * 1024;
pub const MAX_MEDIA: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Control {
    Hello {
        version: u16,
        options: Vec<CodecOption>,
        settings: Option<VideoSettings>,
    },
    Accepted {
        option: CodecOption,
        settings: VideoSettings,
    },
    ChangeSettings(VideoSettings),
    Stats {
        frames: u64,
        dropped: u64,
        bitrate_kbps: u32,
    },
    Error(String),
    MediaStatus {
        lane: MediaLane,
        state: MediaState,
        message: String,
    },
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaLane {
    Video,
    Audio,
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum MediaState {
    Starting,
    Ready,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MediaStreamHello {
    pub version: u16,
    pub lane: MediaLane,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaHeader {
    pub kind: MediaKind,
    pub sequence: u64,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u8,
}

pub async fn send_json<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    value: &T,
) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    if data.len() > MAX_CONTROL {
        bail!("Control message is too large");
    }
    stream.write_u32(data.len() as u32).await?;
    stream.write_all(&data).await?;
    Ok(())
}

pub async fn receive_json<R: AsyncRead + Unpin, T: DeserializeOwned>(stream: &mut R) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_CONTROL {
        bail!("Control message exceeds size limit");
    }
    let mut data = vec![0; len];
    stream.read_exact(&mut data).await?;
    serde_json::from_slice(&data).context("Invalid control message")
}

pub async fn send_media<W: AsyncWrite + Unpin>(
    stream: &mut W,
    header: &MediaHeader,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > MAX_MEDIA {
        bail!("Media packet exceeds size limit");
    }
    send_json(stream, header).await?;
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    Ok(())
}

pub async fn receive_media<R: AsyncRead + Unpin>(stream: &mut R) -> Result<(MediaHeader, Vec<u8>)> {
    let header = receive_json(stream).await?;
    let len = stream.read_u32().await? as usize;
    if len > MAX_MEDIA {
        bail!("Media packet exceeds size limit");
    }
    let mut data = vec![0; len];
    stream.read_exact(&mut data).await?;
    Ok((header, data))
}
