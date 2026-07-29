use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ClientMessage, ProtocolLogDirection, ServerMessage, append_protocol_log_record,
    protocol_frame_bytes, read_frame, write_frame,
};

/// Raw async client for the rho UI Unix-socket protocol.
pub struct Client {
    stream: rho_rpc::Stream,
    logger: Option<ProtocolLogger>,
}

impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let stream = rho_rpc::connect_unix(path).await?;
        Ok(Self::from_stream(stream))
    }

    pub fn from_stream(stream: rho_rpc::Stream) -> Self {
        Self {
            stream,
            logger: ProtocolLogger::from_env(),
        }
    }

    pub async fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        write_frame(&mut self.stream, message).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ClientToServer, message);
        }
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<ServerMessage> {
        let message = read_frame(&mut self.stream).await?;
        if let Some(logger) = &self.logger {
            logger.log(ProtocolLogDirection::ServerToClient, &message);
        }
        Ok(message)
    }

    pub fn into_stream(self) -> rho_rpc::Stream {
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
