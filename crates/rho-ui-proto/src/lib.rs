//! Senax-framed Unix-socket protocol shared by rho UI processes.
//!
//! This crate intentionally owns only the wire vocabulary and framing. The CLI
//! and daemon can map these messages onto concrete `rho-agent` handles without
//! teaching lower crates about sockets or UI policy.

use anyhow::{Context as _, bail};
use rho_core::ContentPart;
use senax_encoder::{Decode, Decoder, Encode, Encoder};

pub mod client;
pub mod remote;
pub mod server;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum accepted frame payload size.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

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
    Ok(())
}

/// Read and decode one length-prefixed senax frame.
pub async fn read_frame<R, T>(reader: &mut R) -> anyhow::Result<T>
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
    senax_encoder::decode(&mut payload).context("decode protocol frame")
}

/// Marker tying this protocol layer to `rho-agent` without putting socket code
/// in the agent crate.
pub type Agent = rho_agent::Agent;
