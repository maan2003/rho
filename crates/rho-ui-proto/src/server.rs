use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use crate::{ClientMessage, ServerMessage, read_frame, write_frame};

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
        Ok(ServerConnection { stream })
    }

    pub fn local_addr(&self) -> anyhow::Result<tokio::net::unix::SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}

/// One accepted UI client connection.
pub struct ServerConnection {
    stream: UnixStream,
}

impl ServerConnection {
    pub fn from_stream(stream: UnixStream) -> Self {
        Self { stream }
    }

    pub async fn recv(&mut self) -> anyhow::Result<ClientMessage> {
        read_frame(&mut self.stream).await
    }

    pub async fn send(&mut self, message: &ServerMessage) -> anyhow::Result<()> {
        write_frame(&mut self.stream, message).await
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}
