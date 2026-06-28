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
}
