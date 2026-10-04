//! Versioned AirBridge wire protocol.
//!
//! A QUIC connection has one bidirectional control stream. Each screen's
//! media lanes use independent unidirectional QUIC streams so a slow or
//! restarted video lane does not block control or another screen.

use crate::config::{CodecOption, VideoSettings};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub type ScreenId = u32;
pub type RequestId = u64;
pub type ConfigRevision = u64;
pub type StreamGeneration = u64;

pub const VERSION: u16 = 5;
pub const MAX_VIEWERS_PER_SCREEN: usize = 8;
pub const PORT: u16 = 44990;
pub const MAX_CONTROL: usize = 64 * 1024;
pub const MAX_MEDIA: usize = 8 * 1024 * 1024;
pub const MAX_SCREENS: usize = 8;

/// The first message sent by the viewer on the control stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u16,
    pub codecs: Vec<CodecOption>,
    pub initial_settings: Option<VideoSettings>,
}

/// Complete settings for one screen. Revisions increase whenever settings are
/// applied, while generations increase whenever its media pipeline is rebuilt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenConfig {
    pub screen_id: ScreenId,
    pub name: String,
    pub revision: ConfigRevision,
    pub generation: StreamGeneration,
    pub settings: VideoSettings,
    pub codec: CodecOption,
    #[serde(default)]
    pub instrumented: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddScreenRequest {
    pub request_id: RequestId,
    pub settings: Option<VideoSettings>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeScreenSettingsRequest {
    pub request_id: RequestId,
    pub screen_id: ScreenId,
    pub revision: ConfigRevision,
    pub settings: VideoSettings,
}

/// Viewer-to-host requests. This enum is only read from the control stream on
/// the host; role-specific types make invalid directionality explicit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(ClientHello),
    AddScreen(AddScreenRequest),
    ChangeScreenSettings(ChangeScreenSettingsRequest),
    RemoveScreen {
        request_id: RequestId,
        screen_id: ScreenId,
    },
    Diagnostics {
        screen_id: ScreenId,
        enabled: bool,
    },
    TraceBurst {
        screen_id: ScreenId,
    },
    ClockProbe {
        t1: u64,
    },
    VideoStopped {
        screen_id: ScreenId,
        generation: StreamGeneration,
    },
    Stop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostMessage {
    SessionAccepted {
        host_id: String,
        viewer_id: u64,
        screen: ScreenConfig,
    },
    Failure(crate::failure::Failure),
    ClockReply {
        t1: u64,
        t2: u64,
        t3: u64,
    },
    Accepted {
        screen: ScreenConfig,
    },
    ScreenAdded {
        request_id: Option<RequestId>,
        screen: ScreenConfig,
    },
    ScreenSettingsApplied {
        request_id: Option<RequestId>,
        screen: ScreenConfig,
    },
    ScreenRemoved {
        request_id: Option<RequestId>,
        screen_id: ScreenId,
    },
    StreamStatus {
        screen_id: ScreenId,
        generation: StreamGeneration,
        lane: MediaLane,
        state: MediaState,
        message: String,
    },
    Stats {
        screen_id: ScreenId,
        generation: StreamGeneration,
        frames_sent: u64,
        frames_dropped: u64,
        bitrate_kbps: u32,
    },
    Error {
        request_id: Option<RequestId>,
        screen_id: Option<ScreenId>,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaLane {
    Diagnostics,
    Video,
    Audio,
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    Starting,
    WaitingForFirstFrame,
    Restarting,
    Stopping,
    Ready,
    Failed,
    Stopped,
}

/// Sent at the start of every QUIC unidirectional media stream.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MediaStreamHello {
    pub version: u16,
    pub screen_id: ScreenId,
    pub generation: StreamGeneration,
    pub lane: MediaLane,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Video,
    Audio,
}

/// Per-packet metadata. Screen and generation IDs let the receiver discard
/// late packets from a pipeline that was replaced during a live settings edit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaHeader {
    pub screen_id: ScreenId,
    pub generation: StreamGeneration,
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

/// Own framing in one persistent task: cancelling a select branch must never
/// discard a partially read control message. The bounded queue supplies backpressure.
pub fn control_reader<R, T>(
    mut stream: R,
) -> (
    tokio::sync::mpsc::Receiver<Result<T>>,
    tokio::task::JoinHandle<()>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
    T: DeserializeOwned + Send + 'static,
{
    let (send, receive) = tokio::sync::mpsc::channel(16);
    let task = tokio::spawn(async move {
        loop {
            let message = receive_json(&mut stream).await;
            let ended = message.is_err();
            if send.send(message).await.is_err() || ended {
                break;
            }
        }
    });
    (receive, task)
}

pub async fn send_control<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    value: &T,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), send_json(stream, value))
        .await
        .context("Control channel write timed out")?
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DebugMediaHeader {
    pub header: MediaHeader,
    pub capture_us: Option<u64>,
    pub send_start_us: u64,
}

pub async fn send_debug_media<W: AsyncWrite + Unpin>(
    stream: &mut W,
    header: &MediaHeader,
    capture_us: Option<u64>,
    send_start_us: u64,
    payload: &[u8],
) -> Result<()> {
    if payload.len() > MAX_MEDIA {
        bail!("Media packet exceeds size limit");
    }
    send_json(
        stream,
        &DebugMediaHeader {
            header: header.clone(),
            capture_us,
            send_start_us,
        },
    )
    .await?;
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    Ok(())
}
pub async fn receive_debug_media_or_end<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<Option<(MediaHeader, Option<u64>, u64, Vec<u8>)>> {
    let mut length = [0; 4];
    if stream.read(&mut length[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL {
        bail!("Media header exceeds size limit");
    }
    let mut header = vec![0; length];
    stream.read_exact(&mut header).await?;
    let header: DebugMediaHeader =
        serde_json::from_slice(&header).context("Invalid diagnostic media header")?;
    let length = stream.read_u32().await? as usize;
    if length > MAX_MEDIA {
        bail!("Media packet exceeds size limit");
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(Some((
        header.header,
        header.capture_us,
        header.send_start_us,
        payload,
    )))
}

/// A FIN between packets is a graceful lane end; a FIN inside a packet is an error.
pub async fn receive_media_or_end<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<Option<(MediaHeader, Vec<u8>)>> {
    let mut length = [0; 4];
    if stream.read(&mut length[..1]).await? == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_CONTROL {
        bail!("Media header exceeds size limit");
    }
    let mut header = vec![0; length];
    stream.read_exact(&mut header).await?;
    let header = serde_json::from_slice(&header).context("Invalid media header")?;
    let length = stream.read_u32().await? as usize;
    if length > MAX_MEDIA {
        bail!("Media packet exceeds size limit");
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok(Some((header, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn debug_framing_is_distinct_and_fin_is_checked() {
        let header = MediaHeader {
            screen_id: 1,
            generation: 2,
            kind: MediaKind::Video,
            sequence: 42,
            timestamp_us: 10,
            keyframe: true,
            width: 1920,
            height: 1080,
            sample_rate: 0,
            channels: 0,
        };
        let mut data = Vec::new();
        send_debug_media(&mut data, &header, Some(100), 200, b"video")
            .await
            .unwrap();
        let mut reader = data.as_slice();
        let (h, capture, send, bytes) = receive_debug_media_or_end(&mut reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.sequence, 42);
        assert_eq!(capture, Some(100));
        assert_eq!(send, 200);
        assert_eq!(bytes, b"video");
        assert!(
            receive_debug_media_or_end(&mut reader)
                .await
                .unwrap()
                .is_none()
        );
        assert!(receive_media_or_end(&mut data.as_slice()).await.is_err());
        assert!(
            receive_debug_media_or_end(&mut &data[..data.len() - 1])
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn control_framing_survives_other_ready_select_branches() {
        let (mut writer, reader) = tokio::io::duplex(4);
        let (mut messages, task) = control_reader::<_, ClientMessage>(reader);
        let writing = tokio::spawn(async move {
            for value in [
                ClientMessage::Stop,
                ClientMessage::RemoveScreen {
                    request_id: 19,
                    screen_id: 3,
                },
            ] {
                let data = serde_json::to_vec(&value).unwrap();
                let packet = [u32::to_be_bytes(data.len() as u32).as_slice(), &data].concat();
                for byte in packet {
                    writer.write_all(&[byte]).await.unwrap();
                    tokio::task::yield_now().await;
                }
            }
        });
        let mut messages_seen = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while messages_seen.len() < 2 {
                tokio::select! {
                    value = messages.recv() => messages_seen.push(value.unwrap().unwrap()),
                    _ = tokio::task::yield_now() => {},
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(messages_seen[0], ClientMessage::Stop));
        assert!(matches!(
            messages_seen[1],
            ClientMessage::RemoveScreen {
                request_id: 19,
                screen_id: 3
            }
        ));
        assert!(messages.recv().await.unwrap().is_err());
        writing.await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn media_fin_is_clean_only_between_complete_packets() {
        let header = MediaHeader {
            screen_id: 2,
            generation: 4,
            kind: MediaKind::Video,
            sequence: 1,
            timestamp_us: 0,
            keyframe: true,
            width: 1920,
            height: 1080,
            sample_rate: 0,
            channels: 0,
        };
        let mut encoded = Vec::new();
        send_media(&mut encoded, &header, b"frame").await.unwrap();
        let mut reader = encoded.as_slice();
        let (decoded, data) = receive_media_or_end(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded.generation, 4);
        assert_eq!(data, b"frame");
        assert!(receive_media_or_end(&mut reader).await.unwrap().is_none());
        for end in [1, 3, encoded.len() - 1] {
            assert!(receive_media_or_end(&mut &encoded[..end]).await.is_err());
        }
        let oversized = u32::to_be_bytes(MAX_CONTROL as u32 + 1);
        assert!(
            receive_media_or_end(&mut oversized.as_slice())
                .await
                .is_err()
        );
    }

    #[test]
    fn settings_requests_keep_screen_revision_and_request_identity() {
        let request = ClientMessage::ChangeScreenSettings(ChangeScreenSettingsRequest {
            request_id: 41,
            screen_id: 7,
            revision: 12,
            settings: VideoSettings {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 12_000,
                keyframe_interval: 120,
                h264_profile: Default::default(),
                low_latency_rate_control: true,
                vsync: true,
                audio: false,
                preferred_codec: None,
            },
        });

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["type"], "change_screen_settings");
        assert_eq!(encoded["payload"]["request_id"], 41);
        assert_eq!(encoded["payload"]["screen_id"], 7);
        assert_eq!(encoded["payload"]["revision"], 12);

        let decoded: ClientMessage = serde_json::from_value(encoded).unwrap();
        match decoded {
            ClientMessage::ChangeScreenSettings(request) => {
                assert_eq!(request.request_id, 41);
                assert_eq!(request.screen_id, 7);
                assert_eq!(request.revision, 12);
                assert_eq!(request.settings.width, 1920);
            }
            _ => panic!("decoded a different control message"),
        }
    }

    #[test]
    fn screen_config_acknowledgements_are_revision_and_generation_scoped() {
        let message = HostMessage::ScreenSettingsApplied {
            request_id: Some(73),
            screen: ScreenConfig {
                screen_id: 4,
                name: "Screen 4".into(),
                revision: 18,
                generation: 23,
                settings: VideoSettings {
                    width: 2560,
                    height: 1440,
                    fps: 60,
                    bitrate_kbps: 18_000,
                    keyframe_interval: 120,
                    h264_profile: Default::default(),
                    low_latency_rate_control: true,
                    vsync: true,
                    audio: false,
                    preferred_codec: None,
                },
                instrumented: false,
                codec: crate::config::CodecOption {
                    codec: crate::config::Codec::H264,
                    acceleration: crate::config::Acceleration::Hardware,
                },
            },
        };

        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded["type"], "screen_settings_applied");
        assert_eq!(encoded["payload"]["request_id"], 73);
        assert_eq!(encoded["payload"]["screen"]["screen_id"], 4);
        assert_eq!(encoded["payload"]["screen"]["revision"], 18);
        assert_eq!(encoded["payload"]["screen"]["generation"], 23);

        let decoded: HostMessage = serde_json::from_value(encoded).unwrap();
        match decoded {
            HostMessage::ScreenSettingsApplied { request_id, screen } => {
                assert_eq!(request_id, Some(73));
                assert_eq!(screen.screen_id, 4);
                assert_eq!(screen.revision, 18);
                assert_eq!(screen.generation, 23);
                assert_eq!(screen.settings.width, 2560);
            }
            _ => panic!("decoded a different host response"),
        }
    }

    #[test]
    fn media_lane_preamble_and_packets_are_scoped_to_screen_generation() {
        let preamble = MediaStreamHello {
            version: VERSION,
            screen_id: 3,
            generation: 9,
            lane: MediaLane::Video,
        };
        let encoded = serde_json::to_value(preamble).unwrap();
        assert_eq!(encoded["screen_id"], 3);
        assert_eq!(encoded["generation"], 9);
        assert_eq!(encoded["lane"], "video");

        let packet = MediaHeader {
            screen_id: 3,
            generation: 9,
            kind: MediaKind::Video,
            sequence: 150,
            timestamp_us: 2_500_000,
            keyframe: true,
            width: 1920,
            height: 1080,
            sample_rate: 0,
            channels: 0,
        };
        let encoded = serde_json::to_value(packet).unwrap();
        assert_eq!(encoded["screen_id"], 3);
        assert_eq!(encoded["generation"], 9);
        assert_eq!(encoded["sequence"], 150);
    }
}
