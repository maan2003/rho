//! Compressed typed streams shared by Rho's Unix and iroh transports.
//!
//! Every application direction is one long-lived zstd frame. Senax messages
//! retain their bounded length prefix inside that compressed byte stream, so
//! callers can switch to raw bytes after a typed handshake without changing
//! compression layers.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, bail};
use async_compression::codecs::zstd::params::{CParameter, DParameter};
use async_compression::core::Level;
use async_compression::tokio::bufread::ZstdDecoder;
use async_compression::tokio::write::ZstdEncoder;
use futures::{SinkExt as _, StreamExt as _};
use senax_encoder::{Packer, Unpacker};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};

/// Zstd's maximum history window in each direction (128 KiB).
///
/// Setting both sides prevents an untrusted compressed header from requesting
/// an attacker-selected decoder allocation while retaining useful history
/// across ordinary UI frames.
pub const ZSTD_WINDOW_LOG: u32 = 17;
/// Fast, general-purpose compression for latency-sensitive application data.
pub const ZSTD_LEVEL: i32 = 3;
#[cfg(unix)]
const UNIX_PREFACE: &[u8; 12] = b"RHO-STREAM-3";
#[cfg(unix)]
const PREFACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(feature = "server")]
const IROH_SERVER_SECRET: redb::TableDefinition<(), &[u8; 32]> =
    redb::TableDefinition::new("rho_daemon_iroh_secret_v1");

/// Binds the native GUI's process-ephemeral iroh identity with Rho's standard
/// incoming agent-stream credit and qlog policy.
#[cfg(feature = "native-client")]
pub async fn bind_ephemeral_iroh_client() -> anyhow::Result<iroh::Endpoint> {
    install_crypto_provider()?;
    iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(iroh::SecretKey::generate())
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .max_concurrent_uni_streams(1024u32.into())
                .qlog_from_env("rho-gui")
                .build(),
        )
        .bind()
        .await
        .context("bind ephemeral iroh client endpoint")
}

/// Binds the browser client's externally derived persistent identity with the
/// bounded stream credit used for selected-agent replacement overlap.
#[cfg(feature = "browser-client")]
pub async fn bind_browser_iroh_client(secret: iroh::SecretKey) -> anyhow::Result<iroh::Endpoint> {
    iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .transport_config(
            iroh::endpoint::QuicTransportConfig::builder()
                .max_concurrent_uni_streams(16u32.into())
                .build(),
        )
        .bind()
        .await
        .context("bind browser iroh client endpoint")
}

/// Performs the mandatory raw, bounded authentication exchange before any
/// compressed application stream can be opened.
pub async fn authenticate_iroh_client(
    connection: &iroh::endpoint::Connection,
    client_endpoint_id: iroh::EndpointId,
) -> anyhow::Result<rho_iroh_auth::ClientAuthResult> {
    tokio::time::timeout(
        AUTH_TIMEOUT,
        rho_iroh_auth::authenticate_client(connection, client_endpoint_id),
    )
    .await
    .map_err(|_| anyhow::anyhow!("iroh authentication timed out"))?
}

/// Performs the server side of the mandatory raw authentication exchange.
/// Callers may accept compressed application streams only after `Approved`.
#[cfg(feature = "server")]
async fn authenticate_iroh_server(
    auth: &rho_iroh_auth::IrohAuth,
    connection: &iroh::endpoint::Connection,
    preapproved: Option<rho_iroh_auth::PreapprovedEndpoint>,
) -> anyhow::Result<rho_iroh_auth::ServerAuthDecision> {
    rho_iroh_auth::authenticate_server_connection(auth, connection, preapproved).await
}

/// Accepts iroh connections and exposes only peers that completed the
/// explicit auth exchange with an approved endpoint identity.
///
/// Trusted reconnects bypass the enrollment semaphore. Unknown peers share a
/// bounded admission pool and cannot invoke compressed application parsing.
#[cfg(feature = "server")]
pub struct AuthenticatedIrohListener {
    endpoint: iroh::Endpoint,
    auth: rho_iroh_auth::IrohAuth,
    enrollments: std::sync::Arc<tokio::sync::Semaphore>,
    attempts: tokio::task::JoinSet<anyhow::Result<Option<iroh::endpoint::Connection>>>,
    accepting: bool,
    approved_bi_streams: u32,
}

#[cfg(feature = "server")]
impl AuthenticatedIrohListener {
    /// Loads the persistent endpoint identity, configures the transport, and
    /// binds an authenticated listener for the supplied application ALPN.
    pub async fn bind(
        db: rho_db::RhoDb,
        alpn: impl Into<Vec<u8>>,
    ) -> anyhow::Result<(Self, rho_iroh_auth::IrohAuth)> {
        install_crypto_provider()?;
        let secret = load_or_create_server_secret(&db).await?;
        let auth = rho_iroh_auth::IrohAuth::new(db, secret.public());
        let mut transport = iroh::endpoint::QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(16u8.into())
            .qlog_from_env("rho-daemon");
        if env_flag("RHO_IROH_BBR3") {
            transport = transport.congestion_controller_factory(std::sync::Arc::new(
                noq_proto::congestion::Bbr3Config::default(),
            ));
        }
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
            .transport_config(transport.build())
            .alpns(vec![alpn.into()])
            .bind()
            .await
            .context("bind authenticated iroh endpoint")?;
        let listener = Self::new(endpoint, auth.clone(), 1024)?;
        Ok((listener, auth))
    }

    fn new(
        endpoint: iroh::Endpoint,
        auth: rho_iroh_auth::IrohAuth,
        approved_bi_streams: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            endpoint.id() == auth.server_endpoint_id(),
            "iroh endpoint identity does not match authentication identity"
        );
        anyhow::ensure!(
            approved_bi_streams > 0,
            "approved stream credit must be nonzero"
        );
        Ok(Self {
            endpoint,
            auth,
            enrollments: std::sync::Arc::new(tokio::sync::Semaphore::new(64)),
            attempts: tokio::task::JoinSet::new(),
            accepting: true,
            approved_bi_streams,
        })
    }

    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    /// Returns the next approved connection. Individual handshake failures
    /// are reported without stopping the listener.
    pub async fn accept(&mut self) -> Option<anyhow::Result<iroh::endpoint::Connection>> {
        loop {
            if !self.accepting && self.attempts.is_empty() {
                return None;
            }
            if self.attempts.is_empty() {
                let incoming = self.endpoint.accept().await?;
                self.spawn_attempt(incoming);
                continue;
            }
            tokio::select! {
                biased;
                completed = self.attempts.join_next() => {
                    if let Some(result) = completed.and_then(finish_auth_attempt) {
                        return Some(result);
                    }
                }
                incoming = self.endpoint.accept(), if self.accepting => {
                    match incoming {
                        Some(incoming) => self.spawn_attempt(incoming),
                        None => self.accepting = false,
                    }
                }
            }
        }
    }

    fn spawn_attempt(&mut self, incoming: iroh::endpoint::Incoming) {
        self.attempts.spawn(authenticate_incoming_iroh(
            incoming,
            self.auth.clone(),
            self.enrollments.clone(),
            self.approved_bi_streams,
        ));
    }

    /// Gracefully closes the endpoint and cancels outstanding authentication
    /// attempts. Dropping the listener aborts attempts without waiting.
    pub async fn close(mut self) {
        self.endpoint.close().await;
        self.attempts.shutdown().await;
    }
}

#[cfg(any(feature = "server", feature = "native-client"))]
fn install_crypto_provider() -> anyhow::Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the AWS-LC rustls crypto provider"))?;
    }
    Ok(())
}

#[cfg(feature = "server")]
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(feature = "server")]
async fn load_or_create_server_secret(db: &rho_db::RhoDb) -> anyhow::Result<iroh::SecretKey> {
    let mut write = db.write().await;
    let mut table = write.open_table(IROH_SERVER_SECRET);
    if let Some(secret) = table.get(&()) {
        return Ok(iroh::SecretKey::from_bytes(secret.value()));
    }

    let secret = iroh::SecretKey::generate().to_bytes();
    table.insert(&(), &secret);
    drop(table);
    write.commit();
    Ok(iroh::SecretKey::from_bytes(&secret))
}

#[cfg(feature = "server")]
fn finish_auth_attempt(
    joined: Result<anyhow::Result<Option<iroh::endpoint::Connection>>, tokio::task::JoinError>,
) -> Option<anyhow::Result<iroh::endpoint::Connection>> {
    match joined {
        Ok(Ok(Some(connection))) => Some(Ok(connection)),
        Ok(Ok(None)) => None,
        Ok(Err(error)) => Some(Err(error)),
        Err(error) => Some(Err(anyhow::anyhow!(
            "iroh authentication task failed: {error}"
        ))),
    }
}

#[cfg(feature = "server")]
async fn authenticate_incoming_iroh(
    incoming: iroh::endpoint::Incoming,
    auth: rho_iroh_auth::IrohAuth,
    enrollments: std::sync::Arc<tokio::sync::Semaphore>,
    approved_bi_streams: u32,
) -> anyhow::Result<Option<iroh::endpoint::Connection>> {
    let connection = incoming.await.context("accept iroh connection")?;
    let preapproved = auth.preapprove_endpoint(connection.remote_id()).await;
    let enrollment_permit = if preapproved.is_some() {
        None
    } else {
        match tokio::time::timeout(AUTH_TIMEOUT, enrollments.acquire_owned()).await {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(_)) => return Ok(None),
            Err(_) => {
                connection.close(0u32.into(), b"iroh enrollment capacity unavailable");
                return Ok(None);
            }
        }
    };
    match authenticate_iroh_server(&auth, &connection, preapproved).await {
        Ok(rho_iroh_auth::ServerAuthDecision::Approved) => {
            connection.set_max_concurrent_bi_streams(approved_bi_streams.into());
            drop(enrollment_permit);
            Ok(Some(connection))
        }
        Ok(
            rho_iroh_auth::ServerAuthDecision::EnrollmentRequired(_)
            | rho_iroh_auth::ServerAuthDecision::Unavailable,
        ) => {
            connection.close(0u32.into(), b"iroh authentication required");
            Ok(None)
        }
        Err(error) => {
            connection.close(0u32.into(), b"iroh authentication failed");
            Err(error.context("authenticate iroh connection"))
        }
    }
}

type BoxReader = Box<dyn AsyncRead + Unpin + Send>;
type BoxWriter = Box<dyn AsyncWrite + Unpin + Send>;

/// Decompressed receive half of one application stream.
pub struct Reader {
    inner: ZstdDecoder<BufReader<BoxReader>>,
}

impl Reader {
    pub fn new<R>(reader: R) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let reader = BufReader::new(Box::new(reader) as BoxReader);
        Self {
            inner: ZstdDecoder::with_params(reader, &[DParameter::window_log_max(ZSTD_WINDOW_LOG)]),
        }
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
    }
}

/// Compressed send half of one application stream.
pub struct Writer {
    inner: ZstdEncoder<BoxWriter>,
}

impl Writer {
    pub fn new<W>(writer: W) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            inner: ZstdEncoder::with_quality_and_params(
                Box::new(writer) as BoxWriter,
                Level::Precise(ZSTD_LEVEL),
                &[
                    CParameter::window_log(ZSTD_WINDOW_LOG),
                    CParameter::content_size_flag(false),
                ],
            ),
        }
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A bidirectional compressed application stream.
pub struct Stream {
    reader: Reader,
    writer: Writer,
}

impl Stream {
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            reader: Reader::new(reader),
            writer: Writer::new(writer),
        }
    }

    pub fn into_split(self) -> (Reader, Writer) {
        (self.reader, self.writer)
    }

    /// Starts one supervised, bounded typed pump after any service-specific
    /// handshake has completed.
    pub fn into_channel<Tx, Rx>(self, config: ChannelConfig) -> FramedChannel<Tx, Rx>
    where
        Tx: Packer + Send + Sync + 'static,
        Rx: Unpacker + Send + 'static,
    {
        assert!(config.tx_capacity > 0, "tx capacity must be nonzero");
        assert!(config.rx_capacity > 0, "rx capacity must be nonzero");
        let (mut reader, mut writer) = self.into_split();
        let (outgoing, mut outgoing_rx) = futures::channel::mpsc::channel(config.tx_capacity);
        let (mut incoming_tx, incoming) = futures::channel::mpsc::channel(config.rx_capacity);
        let task = tokio::spawn(async move {
            let mut writer_errors = incoming_tx.clone();
            let reader_loop = async move {
                loop {
                    match read_frame::<_, Rx>(&mut reader, config.rx_limit).await {
                        Ok((message, _)) => incoming_tx
                            .send(Ok(message))
                            .await
                            .map_err(|_| anyhow::anyhow!("framed channel receiver dropped"))?,
                        Err(error) => {
                            let message = error.to_string();
                            let _ = incoming_tx.send(Err(error)).await;
                            anyhow::bail!(message);
                        }
                    }
                }
            };
            let writer_loop = async move {
                while let Some(message) = outgoing_rx.next().await {
                    if let Err(error) = write_frame(&mut writer, &message, config.tx_limit).await {
                        let message = error.to_string();
                        let _ = writer_errors.send(Err(error)).await;
                        anyhow::bail!(message);
                    }
                }
                writer.shutdown().await.context("close framed channel")
            };
            // The loops are independently polled: backpressure in either
            // direction never disables progress in the reverse direction.
            let _: anyhow::Result<((), ())> = tokio::try_join!(reader_loop, writer_loop);
        });
        FramedChannel {
            outgoing,
            incoming,
            task: ChannelTask { task: Some(task) },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChannelConfig {
    pub tx_limit: usize,
    pub rx_limit: usize,
    pub tx_capacity: usize,
    pub rx_capacity: usize,
}

pub struct FramedChannel<Tx, Rx> {
    outgoing: futures::channel::mpsc::Sender<Tx>,
    incoming: futures::channel::mpsc::Receiver<anyhow::Result<Rx>>,
    task: ChannelTask,
}

impl<Tx, Rx> FramedChannel<Tx, Rx> {
    pub fn into_parts(
        self,
    ) -> (
        futures::channel::mpsc::Sender<Tx>,
        futures::channel::mpsc::Receiver<anyhow::Result<Rx>>,
        ChannelTask,
    ) {
        (self.outgoing, self.incoming, self.task)
    }
}

/// Aborts both channel directions when the owning application surface drops.
pub struct ChannelTask {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ChannelTask {
    /// Waits for a sender-driven graceful half-close and peer EOF.
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        self.task.take().expect("channel task already joined").await
    }
}

impl Drop for ChannelTask {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().reader).poll_read(cx, buffer)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().writer).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

/// A reusable way to open an application stream over either supported
/// transport. Authentication and endpoint ownership remain with the caller.
#[derive(Clone)]
pub enum Dialer {
    #[cfg(unix)]
    Unix(std::path::PathBuf),
    Iroh(iroh::endpoint::Connection),
}

impl Dialer {
    pub async fn open(&self, priority: Option<i32>) -> anyhow::Result<Stream> {
        match self {
            #[cfg(unix)]
            Self::Unix(path) => connect_unix(path).await,
            Self::Iroh(connection) => {
                let (send, recv) = connection.open_bi().await.context("open iroh stream")?;
                if let Some(priority) = priority {
                    send.set_priority(priority)
                        .context("set iroh stream priority")?;
                }
                Ok(Stream::new(recv, send))
            }
        }
    }
}

/// Connects and version-negotiates a compressed Unix application stream.
#[cfg(unix)]
pub async fn connect_unix(path: impl AsRef<std::path::Path>) -> anyhow::Result<Stream> {
    let path = path.as_ref();
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to {}", path.display()))?;
    negotiate_unix(stream).await
}

/// Version-negotiates an already accepted Unix application stream.
#[cfg(unix)]
pub async fn accept_unix(stream: tokio::net::UnixStream) -> anyhow::Result<Stream> {
    negotiate_unix(stream).await
}

#[cfg(unix)]
async fn negotiate_unix(mut stream: tokio::net::UnixStream) -> anyhow::Result<Stream> {
    tokio::time::timeout(PREFACE_TIMEOUT, async {
        // Symmetric write-then-read avoids assigning protocol roles to socket
        // halves while the tiny fixed preface cannot fill either send buffer.
        stream
            .write_all(UNIX_PREFACE)
            .await
            .context("write Unix stream preface")?;
        stream.flush().await.context("flush Unix stream preface")?;
        let mut peer = [0; UNIX_PREFACE.len()];
        stream
            .read_exact(&mut peer)
            .await
            .context("read Unix stream preface")?;
        anyhow::ensure!(peer == *UNIX_PREFACE, "incompatible Rho stream protocol");
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("Unix stream preface timed out"))??;
    let (reader, writer) = stream.into_split();
    Ok(Stream::new(reader, writer))
}

/// Packs and writes one bounded, length-prefixed Senax frame. The returned
/// length is the uncompressed payload length.
pub async fn write_frame<W, T>(writer: &mut W, value: &T, max_len: usize) -> anyhow::Result<usize>
where
    W: AsyncWrite + Unpin,
    T: Packer,
{
    let payload = senax_encoder::pack(value).context("pack protocol frame")?;
    if payload.len() > max_len {
        bail!("protocol frame length {} exceeds {max_len}", payload.len());
    }
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
    // A streaming encoder may otherwise retain a complete small message and
    // deadlock a request/response exchange waiting on its peer.
    writer.flush().await.context("flush frame")?;
    Ok(payload.len())
}

/// Reads and decodes one bounded Senax frame. The returned length is the
/// uncompressed payload length.
pub async fn read_frame<R, T>(reader: &mut R, max_len: usize) -> anyhow::Result<(T, usize)>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
{
    let (payload, ()) = read_frame_with(reader, max_len, |_| async {}).await?;
    let len = payload.len();
    let mut payload = payload.as_slice();
    let value = senax_encoder::unpack(&mut payload).context("unpack protocol frame")?;
    anyhow::ensure!(payload.is_empty(), "trailing bytes in protocol frame");
    Ok((value, len))
}

/// Reads one bounded frame while allowing a caller to reserve its declared
/// uncompressed allocation before memory is committed.
pub async fn read_frame_with<R, F, Fut, A>(
    reader: &mut R,
    max_len: usize,
    reserve: F,
) -> anyhow::Result<(Vec<u8>, A)>
where
    R: AsyncRead + Unpin,
    F: FnOnce(usize) -> Fut,
    Fut: Future<Output = A>,
{
    read_frame_with_optional(reader, max_len, reserve)
        .await?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into())
}

/// Reads one bounded frame, returning `None` only for clean EOF before any
/// bytes of the next decompressed length prefix.
pub async fn read_frame_with_optional<R, F, Fut, A>(
    reader: &mut R,
    max_len: usize,
    reserve: F,
) -> anyhow::Result<Option<(Vec<u8>, A)>>
where
    R: AsyncRead + Unpin,
    F: FnOnce(usize) -> Fut,
    Fut: Future<Output = A>,
{
    let mut prefix = [0_u8; size_of::<u32>()];
    let first = reader
        .read(&mut prefix[..1])
        .await
        .context("read frame length")?;
    if first == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut prefix[1..])
        .await
        .context("read frame length")?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > max_len {
        bail!("protocol frame length {len} exceeds {max_len}");
    }
    let allocation = reserve(len).await;
    let mut payload = vec![0; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read frame payload")?;
    Ok(Some((payload, allocation)))
}

/// Reads and decodes one bounded frame while retaining a caller-provided
/// allocation reservation for the decoded value's lifetime.
pub async fn read_frame_allocated<R, T, F, Fut, A>(
    reader: &mut R,
    max_len: usize,
    reserve: F,
) -> anyhow::Result<(T, A, usize)>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
    F: FnOnce(usize) -> Fut,
    Fut: Future<Output = A>,
{
    let (payload, allocation) = read_frame_with(reader, max_len, reserve).await?;
    let len = payload.len();
    let mut payload = payload.as_slice();
    let value = senax_encoder::unpack(&mut payload).context("unpack protocol frame")?;
    anyhow::ensure!(payload.is_empty(), "trailing bytes in protocol frame");
    Ok((value, allocation, len))
}

pub async fn read_frame_allocated_optional<R, T, F, Fut, A>(
    reader: &mut R,
    max_len: usize,
    reserve: F,
) -> anyhow::Result<Option<(T, A, usize)>>
where
    R: AsyncRead + Unpin,
    T: Unpacker,
    F: FnOnce(usize) -> Fut,
    Fut: Future<Output = A>,
{
    let Some((payload, allocation)) = read_frame_with_optional(reader, max_len, reserve).await?
    else {
        return Ok(None);
    };
    let len = payload.len();
    let mut payload = payload.as_slice();
    let value = senax_encoder::unpack(&mut payload).context("unpack protocol frame")?;
    anyhow::ensure!(payload.is_empty(), "trailing bytes in protocol frame");
    Ok(Some((value, allocation, len)))
}

/// Copies raw stream bytes while flushing every chunk and half-closing the
/// destination at EOF. This is required when the destination is a streaming
/// compressor and the byte protocol has request/response boundaries unknown
/// to this layer.
pub async fn copy_flush<R, W>(reader: &mut R, writer: &mut W) -> anyhow::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await.context("read raw stream")?;
        if read == 0 {
            writer.shutdown().await.context("half-close raw stream")?;
            return Ok(total);
        }
        writer
            .write_all(&buffer[..read])
            .await
            .context("write raw stream")?;
        writer.flush().await.context("flush raw stream")?;
        total = total.saturating_add(read as u64);
    }
}

/// Relays two raw byte streams in both directions with compression-safe flush
/// and half-close semantics.
pub async fn relay_bidirectional<A, B>(a: A, b: B) -> anyhow::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    tokio::try_join!(
        copy_flush(&mut a_read, &mut b_write),
        copy_flush(&mut b_read, &mut a_write),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_config() -> ChannelConfig {
        ChannelConfig {
            tx_limit: 128 * 1024,
            rx_limit: 1024,
            tx_capacity: 1,
            rx_capacity: 1,
        }
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn server_secret_is_persisted_in_database() {
        let temp = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(temp.path().join("rho.redb"));

        let first = load_or_create_server_secret(&db).await.unwrap();
        let second = load_or_create_server_secret(&db).await.unwrap();

        assert_eq!(first.public(), second.public());
    }

    #[tokio::test]
    async fn frames_round_trip_before_stream_shutdown() {
        let (left, right) = tokio::io::duplex(4096);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let mut sender = Stream::new(left_read, left_write);
        let mut receiver = Stream::new(right_read, right_write);

        write_frame(&mut sender, &"hello zstd".to_owned(), 1024)
            .await
            .unwrap();
        let (message, _): (String, _) = read_frame(&mut receiver, 1024).await.unwrap();
        assert_eq!(message, "hello zstd");
    }

    #[tokio::test]
    async fn declared_frame_limit_is_checked_after_decompression() {
        let (left, right) = tokio::io::duplex(4096);
        let (_, mut writer) = Stream::new(tokio::io::empty(), left).into_split();
        let (mut reader, _) = Stream::new(right, tokio::io::sink()).into_split();
        writer.write_u32_le(33).await.unwrap();
        writer.flush().await.unwrap();

        let error = read_frame::<_, String>(&mut reader, 32).await.unwrap_err();
        assert!(error.to_string().contains("exceeds 32"));
    }

    #[tokio::test]
    async fn raw_copy_flushes_before_source_eof() {
        let (source_client, source_relay) = tokio::io::duplex(4096);
        let (wire_send, wire_recv) = tokio::io::duplex(4096);
        let (_, mut compressed) = Stream::new(tokio::io::empty(), wire_send).into_split();
        let (mut decompressed, _) = Stream::new(wire_recv, tokio::io::sink()).into_split();
        let (source_read, mut source_write) = tokio::io::split(source_client);
        let copy = tokio::spawn(async move {
            let mut source_relay = source_relay;
            copy_flush(&mut source_relay, &mut compressed).await
        });

        source_write.write_all(b"pkt").await.unwrap();
        source_write.flush().await.unwrap();
        let mut received = [0; 3];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            decompressed.read_exact(&mut received),
        )
        .await
        .expect("chunk must arrive before EOF")
        .unwrap();
        assert_eq!(&received, b"pkt");

        drop(source_read);
        drop(source_write);
        copy.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn channel_receives_while_outbound_write_is_blocked() {
        let (left, right) = tokio::io::duplex(64);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let FramedChannel {
            mut outgoing,
            mut incoming,
            task,
        } = Stream::new(left_read, left_write).into_channel::<String, String>(channel_config());
        // This deliberately compresses poorly and is much larger than the
        // undrained 64-byte outbound transport buffer.
        let outbound: String = (0..64 * 1024)
            .map(|index| char::from_u32(33 + ((index * 7919) % 90) as u32).unwrap())
            .collect();
        outgoing.send(outbound).await.unwrap();
        tokio::task::yield_now().await;

        let mut peer = Stream::new(right_read, right_write);
        write_frame(&mut peer, &"inbound still progresses".to_owned(), 1024)
            .await
            .unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), incoming.next())
            .await
            .expect("outbound backpressure must not disable inbound polling")
            .unwrap()
            .unwrap();
        assert_eq!(received, "inbound still progresses");
        drop(task);
    }

    #[tokio::test]
    async fn channel_join_finishes_zstd_but_drop_cancels() {
        let (left, right) = tokio::io::duplex(4096);
        let (left_read, left_write) = tokio::io::split(left);
        let (right_read, right_write) = tokio::io::split(right);
        let (outgoing, mut incoming, task) = Stream::new(left_read, left_write)
            .into_channel::<String, String>(channel_config())
            .into_parts();
        let peer = tokio::spawn(async move {
            let mut peer = Stream::new(right_read, right_write);
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            peer.shutdown().await.unwrap();
        });
        drop(outgoing);
        // Retain and drain the receive queue so the reader loop can observe
        // the peer's matching half-close rather than being cancelled.
        let drain = tokio::spawn(async move { while incoming.next().await.is_some() {} });
        tokio::time::timeout(std::time::Duration::from_secs(1), task.join())
            .await
            .expect("graceful channel join timed out")
            .unwrap();
        peer.await.unwrap();
        drain.await.unwrap();

        let (left, mut right) = tokio::io::duplex(4096);
        let (left_read, left_write) = tokio::io::split(left);
        let (_, _, task) = Stream::new(left_read, left_write)
            .into_channel::<String, String>(channel_config())
            .into_parts();
        drop(task);
        let mut byte = [0];
        let read = tokio::time::timeout(std::time::Duration::from_secs(1), right.read(&mut byte))
            .await
            .expect("dropping ChannelTask must cancel its transport")
            .unwrap();
        assert_eq!(read, 0);
    }
}
