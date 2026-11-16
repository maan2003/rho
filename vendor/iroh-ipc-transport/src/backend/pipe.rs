//! Windows message-mode pipes. Every write is one source-tagged QUIC datagram.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use iroh::endpoint::transports::{RecvInfo, Transmit};
use iroh::{EndpointId, SecretKey};
use iroh_base::CustomAddr;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use super::pipe_packet::{HEADER_BYTES, MAX_PACKET, Packet, TOKEN_BYTES};
use crate::{IPC_TRANSPORT_ID, LocalDiscovery};

const MAX_PEERS: usize = 128;
const MAX_MESSAGE: usize = MAX_PACKET + HEADER_BYTES;

type Connecting = Pin<Box<dyn Future<Output = io::Result<NamedPipeClient>> + Send + Sync>>;

enum Connection {
    Connecting(Connecting),
    Ready(NamedPipeClient),
}

struct Peer {
    connection: Connection,
    last_used: Instant,
}

pub(crate) struct Backend {
    token: [u8; TOKEN_BYTES],
    // bounded: MAX_PEERS outbound handles and MAX_PEERS inbound reader tasks.
    peers: HashMap<[u8; TOKEN_BYTES], Peer>,
    incoming: mpsc::Receiver<io::Result<Packet>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessagePipe")
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

impl Backend {
    pub(crate) async fn bind(_discovery: &LocalDiscovery, _id: EndpointId) -> io::Result<Self> {
        let token = *SecretKey::generate().public().as_bytes();
        let server = server(token, true).await?;
        let (send, incoming) = mpsc::channel(32);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(error) = accept(token, server, send.clone(), stopped).await {
                let _ = send.send(Err(error)).await;
            }
        });
        Ok(Self {
            token,
            peers: HashMap::new(),
            incoming,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub(crate) fn local_addr(&self) -> CustomAddr {
        address(self.token)
    }
    pub(crate) fn valid_addr(addr: &CustomAddr) -> bool {
        token(addr).is_some()
    }
    pub(crate) fn max_transmit_segments(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    pub(crate) async fn close(mut self) {
        self.peers.clear();
        self.incoming.close();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    pub(crate) fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        dst: &CustomAddr,
        transmit: &Transmit<'_>,
    ) -> Poll<io::Result<()>> {
        let Some(destination) = token(dst) else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid local pipe address",
            )));
        };
        if transmit.contents.len() > MAX_PACKET || transmit.segment_size.is_some() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe requires one bounded datagram",
            )));
        }
        if !self.peers.contains_key(&destination) {
            self.peers
                .retain(|_, peer| peer.last_used.elapsed() < Duration::from_secs(60));
            if self.peers.len() == MAX_PEERS {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "IPC pipe peer limit reached",
                )));
            }
            self.peers.insert(
                destination,
                Peer {
                    connection: Connection::Connecting(Box::pin(connect(destination))),
                    last_used: Instant::now(),
                },
            );
        }
        let peer = self
            .peers
            .get_mut(&destination)
            .expect("inserted pipe peer");
        peer.last_used = Instant::now();
        if let Connection::Connecting(future) = &mut peer.connection {
            match ready!(future.as_mut().poll(cx)) {
                Ok(client) => peer.connection = Connection::Ready(client),
                Err(error) => {
                    self.peers.remove(&destination);
                    return Poll::Ready(Err(error));
                }
            }
        }
        let Connection::Ready(client) = &peer.connection else {
            unreachable!()
        };
        // bounded: one native message, with a routing token. TLS still verifies
        // identity; the token only replaces recv_from's source address.
        let bytes = Packet::encode(self.token, transmit.contents)?;
        loop {
            ready!(client.poll_write_ready(cx))?;
            match client.try_write(&bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(count) if count == bytes.len() => return Poll::Ready(Ok(())),
                Ok(_) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "partial pipe message",
                    )));
                }
                Err(error) => {
                    self.peers.remove(&destination);
                    return Poll::Ready(Err(error));
                }
            }
        }
    }

    pub(crate) fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [noq_udp::RecvMeta],
        infos: &mut [RecvInfo],
    ) -> Poll<io::Result<usize>> {
        let cap = bufs.len().min(metas.len()).min(infos.len());
        let mut count = 0;
        while count < cap {
            let packet = match self.incoming.poll_recv(cx) {
                Poll::Pending if count == 0 => return Poll::Pending,
                Poll::Pending => break,
                Poll::Ready(None) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "pipe listener closed",
                    )));
                }
                Poll::Ready(Some(packet)) => packet?,
            };
            let len = packet.contents.len();
            if len > bufs[count].len() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pipe packet exceeds receive buffer",
                )));
            }
            bufs[count][..len].copy_from_slice(&packet.contents);
            metas[count] = noq_udp::RecvMeta::default();
            metas[count].len = len;
            metas[count].stride = len;
            infos[count] = RecvInfo::new(address(packet.source), Some(self.local_addr()));
            count += 1;
        }
        Poll::Ready(Ok(count))
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn server(token: [u8; TOKEN_BYTES], first: bool) -> io::Result<NamedPipeServer> {
    // Audited async boundary for CreateNamedPipeW (namespace/resource creation).
    tokio::task::spawn_blocking(move || {
        ServerOptions::new()
            .pipe_mode(PipeMode::Message)
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .max_instances(MAX_PEERS + 1)
            .in_buffer_size(MAX_MESSAGE as u32)
            .out_buffer_size(MAX_MESSAGE as u32)
            .create(name(token))
    })
    .await
    .map_err(io::Error::other)?
}

async fn connect(token: [u8; TOKEN_BYTES]) -> io::Result<NamedPipeClient> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // Audited async boundary for local CreateFileW. No remote pipe
            // names are accepted, so this cannot connect through SMB.
            let result = tokio::task::spawn_blocking(move || {
                ClientOptions::new()
                    .pipe_mode(PipeMode::Message)
                    .open(name(token))
            })
            .await
            .map_err(io::Error::other)?;
            match result {
                Err(error) if error.raw_os_error() == Some(231) => {
                    // ERROR_PIPE_BUSY
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                result => return result,
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local pipe busy"))?
}

async fn accept(
    token: [u8; TOKEN_BYTES],
    mut listener: NamedPipeServer,
    send: mpsc::Sender<io::Result<Packet>>,
    mut stop: oneshot::Receiver<()>,
) -> io::Result<()> {
    let mut readers = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => { readers.shutdown().await; return Ok(()); }
            result = listener.connect(), if readers.len() < MAX_PEERS => {
                result?;
                let next = server(token, false).await?;
                let connected = std::mem::replace(&mut listener, next);
                readers.spawn(read(connected, send.clone()));
            }
            result = readers.join_next(), if !readers.is_empty() => {
                if let Some(Err(error)) = result { return Err(io::Error::other(error)); }
            }
        }
    }
}

async fn read(mut pipe: NamedPipeServer, send: mpsc::Sender<io::Result<Packet>>) {
    while let Ok(packet) = Packet::read(&mut pipe).await {
        if send.send(Ok(packet)).await.is_err() {
            return;
        }
    }
}

fn address(token: [u8; TOKEN_BYTES]) -> CustomAddr {
    let mut bytes = [3; TOKEN_BYTES + 1];
    bytes[1..].copy_from_slice(&token);
    CustomAddr::from_parts(IPC_TRANSPORT_ID, &bytes)
}

fn token(addr: &CustomAddr) -> Option<[u8; TOKEN_BYTES]> {
    if addr.id() != IPC_TRANSPORT_ID || addr.data().first() != Some(&3) {
        return None;
    }
    addr.data()[1..].try_into().ok()
}

fn name(token: [u8; TOKEN_BYTES]) -> String {
    // bounded: one fixed-size random public token, never a caller-supplied path.
    let suffix: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(r"\\.\pipe\iroh-ipc-{suffix}")
}
