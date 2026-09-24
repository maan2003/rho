use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use senax_encoder::{Packer, Unpacker};
use tokio::io::AsyncWriteExt as _;

use super::{
    Answer, Call, Open, ProtocolLogDirection, ProtocolOpen, append_protocol_log_record,
    protocol_frame_bytes, read_frame, write_frame,
};

/// One call, on a stream of its own over the daemon's Unix socket. A
/// refusal is an error.
pub async fn call<C: Call>(socket: impl AsRef<Path>, call: C) -> anyhow::Result<C::Reply> {
    let mut client = Client::connect(socket).await?;
    client.open(&call.open()).await?;
    client.recv::<Answer<C::Reply>>().await?.into_result()
}

/// Raw async client for one stream over the daemon's Unix socket. The first
/// frame sent is an [`Open`] ([`Client::open`]).
pub struct Client {
    stream: crate::Stream,
    logger: Option<ProtocolLogger>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = crate::connect_unix(path).await?;
        Ok(Self::from_stream(stream))
    }

    pub fn from_stream(stream: crate::Stream) -> Self {
        Self {
            stream,
            logger: ProtocolLogger::from_env(),
        }
    }

    /// Opens the stream for a protocol: the first frame sent.
    pub async fn open<T: ProtocolOpen>(&mut self, open: &T) -> anyhow::Result<()> {
        self.send(&Open::of(open)?).await
    }

    pub async fn send<T: Packer>(&mut self, frame: &T) -> anyhow::Result<()> {
        write_frame(&mut self.stream, frame).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ClientToServer, frame);
        }
        Ok(())
    }

    pub async fn recv<T: Packer + Unpacker>(&mut self) -> anyhow::Result<T> {
        let frame = read_frame(&mut self.stream).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ServerToClient, &frame);
        }
        Ok(frame)
    }

    /// Finishes the client's compressed send stream and half-closes the
    /// connection so the daemon can distinguish a normal exit from truncation.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stream.shutdown().await.map_err(Into::into)
    }

    pub fn into_stream(self) -> crate::Stream {
        self.stream
    }
}

#[derive(Clone)]
struct ProtocolLogger {
    file: Arc<Mutex<std::fs::File>>,
}

impl ProtocolLogger {
    fn from_env() -> Option<Self> {
        let path = std::env::var_os("RHO_UI_PROTO_LOG")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    fn log<T>(&self, direction: ProtocolLogDirection, message: &T)
    where
        T: senax_encoder::Packer,
    {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let Ok(frame) = protocol_frame_bytes(message) else {
            return;
        };
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = append_protocol_log_record(&mut *file, now_ms, direction, &frame);
    }
}
