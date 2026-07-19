use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::net::UnixStream;

use crate::{
    ClientMessage, IoCounters, ProtocolLogDirection, ServerMessage, append_protocol_log_record,
    protocol_frame_bytes, read_frame_counted, write_frame_counted,
};

/// Raw async client for the rho UI Unix-socket protocol.
pub struct Client {
    stream: UnixStream,
    counters: IoCounters,
    logger: Option<ProtocolLogger>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self::from_stream(stream))
    }

    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            counters: IoCounters::default(),
            logger: ProtocolLogger::from_env(),
        }
    }

    pub async fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        write_frame_counted(&mut self.stream, message, Some(&self.counters)).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ClientToServer, message);
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<ServerMessage> {
        let message = read_frame_counted(&mut self.stream, Some(&self.counters)).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ServerToClient, &message);
        }
        Ok(message)
    }

    pub fn io_counters(&self) -> IoCounters {
        self.counters.clone()
    }

    pub fn into_stream(self) -> UnixStream {
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
