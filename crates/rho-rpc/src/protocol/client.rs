use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use senax_encoder::{Packer, Unpacker};
use tokio::io::AsyncWriteExt as _;

#[cfg(feature = "native-client")]
use super::RuntimePaths;
use super::{
    Answer, Call, Open, ProtocolLogDirection, ProtocolOpen, append_protocol_log_record,
    protocol_frame_bytes, read_frame, write_frame,
};

/// One call, on a stream of its own to the agent host in `paths`. A
/// refusal is an error.
#[cfg(feature = "native-client")]
pub async fn call<C: Call>(paths: &RuntimePaths, call: C) -> anyhow::Result<C::Reply> {
    let mut client = Client::connect(paths).await?;
    client.open(&call.open()).await?;
    client.recv::<Answer<C::Reply>>().await?.into_result()
}

/// A stream to the agent host in `paths`, for what is not a call.
#[cfg(feature = "native-client")]
pub async fn connect(paths: &RuntimePaths) -> anyhow::Result<crate::Stream> {
    Ok(Client::connect(paths).await?.into_stream())
}

/// This process's connection to each local host it has reached, dialed
/// again once it has closed. One identity and one handshake per host,
/// however many calls.
#[cfg(feature = "native-client")]
static LOCAL: Mutex<Vec<(RuntimePaths, iroh::endpoint::Connection)>> = Mutex::new(Vec::new());
#[cfg(feature = "native-client")]
static ENDPOINT: tokio::sync::OnceCell<iroh::Endpoint> = tokio::sync::OnceCell::const_new();

#[cfg(feature = "native-client")]
async fn local_connection(paths: &RuntimePaths) -> anyhow::Result<iroh::endpoint::Connection> {
    let held = LOCAL
        .lock()
        .unwrap()
        .iter()
        .find(|(held, connection)| held == paths && connection.close_reason().is_none())
        .map(|(_, connection)| connection.clone());
    if let Some(connection) = held {
        return Ok(connection);
    }
    let endpoint = ENDPOINT
        .get_or_try_init(crate::bind_ephemeral_iroh_client)
        .await?;
    let host = crate::local_host(paths)?;
    let connection = crate::connect_host(endpoint, host, super::IROH_ALPN, Some(paths)).await?;
    match crate::authenticate_iroh_client(&connection, endpoint.id(), None).await? {
        rho_iroh_auth::ClientAuthResult::Approved => {}
        other => anyhow::bail!("the agent host did not trust this process: {other:?}"),
    }
    let mut local = LOCAL.lock().unwrap();
    local.retain(|(held, _)| held != paths);
    local.push((paths.clone(), connection.clone()));
    Ok(connection)
}

/// Raw async client for one stream to the agent host. The first frame sent
/// is an [`Open`] ([`Client::open`]).
pub struct Client {
    stream: crate::Stream,
    logger: Option<ProtocolLogger>,
}

impl Client {
    /// A stream of its own on this process's connection to the host in
    /// `paths`, which must be up.
    #[cfg(feature = "native-client")]
    pub async fn connect(paths: &RuntimePaths) -> anyhow::Result<Self> {
        let connection = local_connection(paths).await?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|error| anyhow::anyhow!("open a stream to the agent host: {error}"))?;
        Ok(Self::from_stream(crate::Stream::new(recv, send)))
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
    /// connection so the agent host can distinguish a normal exit from
    /// truncation.
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
