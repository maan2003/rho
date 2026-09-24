use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

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
        loop {
            let (stream, _) = self.listener.accept().await?;
            // A stale client or incompatible protocol version is scoped to
            // that socket; it must not terminate the daemon's listener.
            if let Ok(connection) = ServerConnection::from_stream(stream).await {
                return Ok(connection);
            }
        }
    }

    pub fn local_addr(&self) -> anyhow::Result<tokio::net::unix::SocketAddr> {
        Ok(self.listener.local_addr()?)
    }
}

/// One accepted UI client connection.
pub struct ServerConnection {
    stream: crate::Stream,
    peer_cred: Option<tokio::net::unix::UCred>,
}

impl ServerConnection {
    pub async fn from_stream(stream: UnixStream) -> anyhow::Result<Self> {
        let peer_cred = stream.peer_cred().ok();
        let stream = crate::accept_unix(stream).await?;
        Ok(Self { stream, peer_cred })
    }

    pub fn peer_cred(&self) -> std::io::Result<tokio::net::unix::UCred> {
        self.peer_cred
            .ok_or_else(|| std::io::Error::other("Unix peer credentials unavailable"))
    }

    pub fn into_stream(self) -> crate::Stream {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt as _;

    use super::*;

    #[tokio::test]
    async fn failed_preface_does_not_stop_listener() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rho-agent-host-proto-{}-{nonce}.sock",
            std::process::id()
        ));
        let server = Server::bind(&path).unwrap();
        let accept = tokio::spawn(async move { server.accept().await.unwrap() });

        let mut stale = UnixStream::connect(&path).await.unwrap();
        stale.write_all(b"old protocol").await.unwrap();
        drop(stale);

        let client = crate::connect_unix(&path).await.unwrap();
        let _connection = accept.await.unwrap();
        drop(client);
        let _ = std::fs::remove_file(path);
    }
}
