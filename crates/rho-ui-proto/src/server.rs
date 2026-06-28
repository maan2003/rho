use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::{ClientMessage, IoCounters, ServerMessage, read_frame_counted, write_frame_counted};

/// Async Unix-socket listener for the rho UI protocol.
pub struct Server {
    listener: UnixListener,
}

impl Server {
    pub fn bind(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let listener = UnixListener::bind(path)?;
        Ok(Self { listener })
    }

    pub fn from_listener(listener: UnixListener) -> Self {
        Self { listener }
    }

    pub async fn accept(&self) -> anyhow::Result<ServerConnection> {
        let (stream, _) = self.listener.accept().await?;
        Ok(ServerConnection::from_stream(stream))
    }

    pub fn local_addr(&self) -> anyhow::Result<tokio::net::unix::SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}

/// One accepted UI client connection.
pub struct ServerConnection {
    stream: UnixStream,
    counters: IoCounters,
}

impl ServerConnection {
    pub fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            counters: IoCounters::default(),
        }
    }

    pub async fn recv(&mut self) -> anyhow::Result<ClientMessage> {
        read_frame_counted(&mut self.stream, Some(&self.counters)).await
    }

    pub async fn send(&mut self, message: &ServerMessage) -> anyhow::Result<()> {
        write_frame_counted(&mut self.stream, message, Some(&self.counters)).await
    }

    pub fn io_counters(&self) -> IoCounters {
        self.counters.clone()
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}
