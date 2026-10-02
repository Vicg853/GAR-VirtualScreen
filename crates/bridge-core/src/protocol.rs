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

pub const VERSION: u16 = 3;
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
    Stop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HostMessage {
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
    Video,
    Audio,
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    Starting,
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

#[cfg(test)]
mod tests {
    use super::*;

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
