//! Bounded local IPC. Only compressed video and numeric diagnostics cross it.
use crate::{
    protocol::{MAX_CONTROL, MAX_MEDIA},
    telemetry::TraceEvent,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Local IPC evolves independently from the network protocol.
pub const VERSION: u16 = 2;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceSeed {
    pub capture_us: u64,
    pub offset_us: i64,
    pub uncertainty_us: Option<u64>,
    pub queued_host_us: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub window: crate::sessions::WindowPlacement,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub presented: u64,
    pub gpu_memory_bytes: u64,
    pub gpu_in_flight: u32,
    pub frame_age_us: Option<u64>,
    pub skipped: u64,
    pub decoder: String,
    pub error: Option<String>,
    pub closed: bool,
    pub user_closed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Message {
    ClockProbe {
        t1: u64,
    },
    ClockReply {
        t1: u64,
        t2: u64,
        t3: u64,
    },
    Show,
    Placement(crate::sessions::WindowPlacement),
    Reset {
        width: u32,
        height: u32,
        instrumented: bool,
    },
    Packet {
        keyframe: bool,
        sequence: u64,
        timestamp_us: u64,
        trace: Option<TraceSeed>,
    },
    Stop,
    Status(Status),
    Diagnostics {
        events: Vec<TraceEvent>,
        lost: u64,
    },
    Stopped,
    Error(String),
    Log(String),
}
pub fn write<W: Write>(writer: &mut W, message: &Message, payload: &[u8]) -> io::Result<()> {
    let json = serde_json::to_vec(message).map_err(io::Error::other)?;
    if json.len() > MAX_CONTROL
        || payload.len() > MAX_MEDIA
        || (!matches!(message, Message::Packet { .. }) && !payload.is_empty())
    {
        return Err(io::Error::other("Invalid worker message size"));
    }
    writer.write_all(&VERSION.to_be_bytes())?;
    writer.write_all(&(json.len() as u32).to_be_bytes())?;
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&json)?;
    writer.write_all(payload)?;
    writer.flush()
}
pub fn read<R: Read>(reader: &mut R) -> io::Result<(Message, Vec<u8>)> {
    let mut version = [0; 2];
    reader.read_exact(&mut version)?;
    if u16::from_be_bytes(version) != VERSION {
        return Err(io::Error::other("Worker protocol version mismatch"));
    }
    let mut lengths = [0; 8];
    reader.read_exact(&mut lengths)?;
    let meta = u32::from_be_bytes(lengths[..4].try_into().unwrap()) as usize;
    let bytes = u32::from_be_bytes(lengths[4..].try_into().unwrap()) as usize;
    if meta == 0 || meta > MAX_CONTROL || bytes > MAX_MEDIA {
        return Err(io::Error::other("Oversized worker message"));
    }
    let mut json = vec![0; meta];
    reader.read_exact(&mut json)?;
    let message: Message = serde_json::from_slice(&json).map_err(io::Error::other)?;
    if !matches!(message, Message::Packet { .. }) && bytes != 0 {
        return Err(io::Error::other("Unexpected worker payload"));
    }
    if let Message::Log(line) = &message {
        if line.len() > 2048 {
            return Err(io::Error::other("Oversized worker log"));
        }
    }
    if let Message::Diagnostics { events, .. } = &message {
        if events.len() > 24 {
            return Err(io::Error::other("Oversized worker diagnostics"));
        }
    }
    let mut payload = vec![0; bytes];
    reader.read_exact(&mut payload)?;
    Ok((message, payload))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_worker_version_is_rejected_before_allocation() {
        assert!(read(&mut 0u16.to_be_bytes().as_slice()).is_err());
    }
    #[test]
    fn worker_logs_are_bounded() {
        let mut wire = vec![];
        write(&mut wire, &Message::Log("x".repeat(2049)), &[]).unwrap();
        assert!(read(&mut wire.as_slice()).is_err());
    }
    #[test]
    fn ipc_bounds_and_truncation() {
        let mut wire = vec![];
        write(
            &mut wire,
            &Message::Packet {
                keyframe: true,
                sequence: 1,
                timestamp_us: 1234,
                trace: None,
            },
            b"h264",
        )
        .unwrap();
        let (message, payload) = read(&mut wire.as_slice()).unwrap();
        assert_eq!(payload, b"h264");
        assert!(matches!(
            message,
            Message::Packet {
                timestamp_us: 1234,
                sequence: 1,
                ..
            }
        ));
        wire.pop();
        assert!(read(&mut wire.as_slice()).is_err());
        assert!(write(&mut vec![], &Message::Stop, b"bad").is_err());
        let mut bad = VERSION.to_be_bytes().to_vec();
        bad.extend_from_slice(&(MAX_CONTROL as u32 + 1).to_be_bytes());
        bad.extend_from_slice(&0u32.to_be_bytes());
        assert!(read(&mut bad.as_slice()).is_err());
    }
}
