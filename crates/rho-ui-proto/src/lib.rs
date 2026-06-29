//! Senax-framed Unix-socket protocol shared by rho UI processes.
//!
//! This crate intentionally owns only the wire vocabulary and framing. The CLI
//! and daemon can map these messages onto concrete `rho-agent` handles without
//! teaching lower crates about sockets or UI policy.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, bail};
use rho_core::ContentPart;
use senax_encoder::{Decode, Decoder, Encode, Encoder};

pub mod client;
pub mod remote;
pub mod server;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;
const FRAME_LEN_BYTES: u64 = size_of::<u32>() as u64;
const PROTOCOL_LOG_MAGIC: &[u8; 4] = b"RUP1";

/// Shared byte counters for one UI protocol connection.
///
/// Counts successful length-prefixed frames on the wire, including the 4-byte
/// little-endian frame length.
#[derive(Clone, Debug, Default)]
pub struct IoCounters {
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl IoCounters {
    pub fn snapshot(&self) -> IoStats {
        IoStats {
            sent: self.sent.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
        }
    }

    fn record_sent(&self, payload_len: usize) {
        self.sent
            .fetch_add(frame_wire_len(payload_len), Ordering::Relaxed);
    }

    fn record_received(&self, payload_len: usize) {
        self.received
            .fetch_add(frame_wire_len(payload_len), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IoStats {
    pub sent: u64,
    pub received: u64,
}

/// Message sent from a UI client to the rho daemon.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ClientMessage {
    Ping,
    Subscribe,
    SendUserMessage { content: Vec<ContentPart> },
    CancelTurn,
}

/// Message sent from the rho daemon to a UI client.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ServerMessage {
    Pong,
    Error { message: String },
    Agent(remote::AgentRemoteFrame),
    TurnCancelled,
}

/// Encode and write one length-prefixed senax frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Encoder,
{
    write_frame_counted(writer, value, None).await
}

/// Encode and write one length-prefixed senax frame, recording bytes on
/// successful completion when counters are supplied.
pub async fn write_frame_counted<W, T>(
    writer: &mut W,
    value: &T,
    counters: Option<&IoCounters>,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Encoder,
{
    let payload = senax_encoder::encode(value).context("encode protocol frame")?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("protocol frame too large")?;
    writer
        .write_u32_le(len)
        .await
        .context("write frame length")?;
    writer
        .write_all(&payload)
        .await
        .context("write frame payload")?;
    writer.flush().await.context("flush frame")?;
    if let Some(counters) = counters {
        counters.record_sent(payload.len());
    }
    Ok(())
}

/// Read and decode one length-prefixed senax frame.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Decoder,
{
    read_frame_counted(reader, None).await
}

/// Read and decode one length-prefixed senax frame, recording bytes on
/// successful completion when counters are supplied.
pub async fn read_frame_counted<R, T>(
    reader: &mut R,
    counters: Option<&IoCounters>,
) -> anyhow::Result<T>
where
    R: AsyncRead + Unpin,
    T: Decoder,
{
    let len = reader.read_u32_le().await.context("read frame length")? as usize;
    if len > MAX_FRAME_LEN {
        bail!("protocol frame length {len} exceeds {MAX_FRAME_LEN}");
    }

    let mut payload = vec![0; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read frame payload")?;
    let mut payload = payload.as_slice();
    let message = senax_encoder::decode(&mut payload).context("decode protocol frame")?;
    if let Some(counters) = counters {
        counters.record_received(len);
    }
    Ok(message)
}

fn frame_wire_len(payload_len: usize) -> u64 {
    FRAME_LEN_BYTES + payload_len as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolLogDirection {
    ClientToServer,
    ServerToClient,
}

impl ProtocolLogDirection {
    fn byte(self) -> u8 {
        match self {
            Self::ClientToServer => 0,
            Self::ServerToClient => 1,
        }
    }

    fn from_byte(byte: u8) -> anyhow::Result<Self> {
        match byte {
            0 => Ok(Self::ClientToServer),
            1 => Ok(Self::ServerToClient),
            _ => bail!("invalid protocol log direction {byte}"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClientToServer => "send",
            Self::ServerToClient => "recv",
        }
    }
}

pub fn protocol_frame_bytes<T>(message: &T) -> anyhow::Result<Vec<u8>>
where
    T: Encoder,
{
    let payload = senax_encoder::encode(message).context("encode protocol log frame")?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("protocol log frame too large")?;
    let mut frame = Vec::with_capacity(size_of::<u32>() + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn append_protocol_log_record(
    writer: &mut impl std::io::Write,
    unix_ms: u128,
    direction: ProtocolLogDirection,
    frame: &[u8],
) -> anyhow::Result<()> {
    let unix_ms: u64 = unix_ms
        .try_into()
        .context("protocol log timestamp overflow")?;
    let len: u32 = frame
        .len()
        .try_into()
        .context("protocol log frame too large")?;
    writer.write_all(PROTOCOL_LOG_MAGIC)?;
    writer.write_all(&unix_ms.to_le_bytes())?;
    writer.write_all(&[direction.byte()])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(frame)?;
    Ok(())
}

pub fn print_protocol_log(
    path: impl AsRef<std::path::Path>,
    output: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let mut input = std::fs::File::open(path).context("open protocol log")?;
    loop {
        let Some((unix_ms, direction, frame)) = read_protocol_log_record(&mut input)? else {
            return Ok(());
        };
        if frame.len() < size_of::<u32>() {
            bail!("protocol log frame shorter than length prefix");
        }
        let payload_len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        let payload = frame
            .get(4..)
            .filter(|payload| payload.len() == payload_len)
            .context("protocol log frame length mismatch")?;
        match direction {
            ProtocolLogDirection::ClientToServer => {
                let mut payload = payload;
                let message: ClientMessage =
                    senax_encoder::decode(&mut payload).context("decode client frame")?;
                writeln!(
                    output,
                    "{unix_ms} {} {}B {message:#?}",
                    direction.label(),
                    frame.len()
                )?;
            }
            ProtocolLogDirection::ServerToClient => {
                let mut payload = payload;
                let message: ServerMessage =
                    senax_encoder::decode(&mut payload).context("decode server frame")?;
                writeln!(
                    output,
                    "{unix_ms} {} {}B {message:#?}",
                    direction.label(),
                    frame.len()
                )?;
            }
        }
    }
}

fn read_protocol_log_record(
    input: &mut impl std::io::Read,
) -> anyhow::Result<Option<(u64, ProtocolLogDirection, Vec<u8>)>> {
    let mut magic = [0; 4];
    match input.read_exact(&mut magic) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read protocol log magic"),
    }
    if &magic != PROTOCOL_LOG_MAGIC {
        bail!("invalid protocol log magic");
    }
    let mut timestamp = [0; 8];
    input
        .read_exact(&mut timestamp)
        .context("read protocol log timestamp")?;
    let unix_ms = u64::from_le_bytes(timestamp);
    let mut direction = [0; 1];
    input
        .read_exact(&mut direction)
        .context("read protocol log direction")?;
    let direction = ProtocolLogDirection::from_byte(direction[0])?;
    let mut len = [0; 4];
    input
        .read_exact(&mut len)
        .context("read protocol log frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    let mut frame = vec![0; len];
    input
        .read_exact(&mut frame)
        .context("read protocol log frame")?;
    Ok(Some((unix_ms, direction, frame)))
}

/// Marker tying this protocol layer to `rho-agent` without putting socket code
/// in the agent crate.
pub type Agent = rho_agent::Agent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_wire_len_includes_length_prefix() {
        assert_eq!(frame_wire_len(0), 4);
        assert_eq!(frame_wire_len(12), 16);
    }

    #[test]
    fn protocol_log_records_full_length_prefixed_frame() {
        let frame = protocol_frame_bytes(&ClientMessage::Ping).unwrap();
        let mut log = Vec::new();
        append_protocol_log_record(&mut log, 123, ProtocolLogDirection::ClientToServer, &frame)
            .unwrap();

        let mut cursor = std::io::Cursor::new(log);
        let (unix_ms, direction, recorded_frame) =
            read_protocol_log_record(&mut cursor).unwrap().unwrap();
        assert_eq!(unix_ms, 123);
        assert_eq!(direction, ProtocolLogDirection::ClientToServer);
        assert_eq!(recorded_frame, frame);

        let mut payload = &recorded_frame[4..];
        let message: ClientMessage = senax_encoder::decode(&mut payload).unwrap();
        assert_eq!(message, ClientMessage::Ping);
    }
}
