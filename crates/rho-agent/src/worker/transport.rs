//! One workset connection, multiplexed without waiting for a runtime.
//! Large messages yield between fragments. Small control traffic has reserved
//! admission; order is preserved within each logical port, not across ports.
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const CHUNK_BYTES: usize = 64 * 1024;
const MAX_MESSAGES: usize = 32;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Encode, Decode)]
pub(super) enum Port {
    Workset,
    Agent(crate::db::AgentId),
    Terminal(u64),
    Shell(u64),
}

pub(super) struct Packet {
    pub port: Port,
    pub bytes: Bytes,
}

#[derive(Encode, Decode)]
struct Chunk {
    port: Port,
    offset: u64,
    last: bool,
    // A raw blob: Vec<u8> tags each byte and can exceed the wire chunk limit.
    bytes: Bytes,
}

struct Outgoing {
    port: Port,
    bytes: Bytes,
    offset: usize,
    control: bool,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(super) struct Sender {
    queue: mpsc::UnboundedSender<Outgoing>,
    control: Arc<Semaphore>,
    data: Arc<Semaphore>,
}

impl Sender {
    pub async fn send(&self, port: Port, bytes: Bytes) -> io::Result<()> {
        let control = bytes.len() <= CHUNK_BYTES;
        let slots = if control { &self.control } else { &self.data };
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.queue
            .send(Outgoing {
                port,
                bytes,
                offset: 0,
                control,
                _permit: permit,
            })
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    pub fn close(&self) {
        self.control.close();
        self.data.close();
    }
}

struct Writer {
    socket: tokio::net::unix::OwnedWriteHalf,
    incoming: mpsc::UnboundedReceiver<Outgoing>,
    control_slots: Arc<Semaphore>,
    data_slots: Arc<Semaphore>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.control_slots.close();
        self.data_slots.close();
    }
}

impl Writer {
    async fn run(&mut self) -> io::Result<()> {
        let mut ports: HashMap<Port, VecDeque<Outgoing>> = HashMap::new();
        let mut order = VecDeque::new();
        let mut control_run = 0;
        loop {
            if order.is_empty() {
                let Some(message) = self.incoming.recv().await else {
                    return Ok(());
                };
                order.push_back(message.port);
                ports.entry(message.port).or_default().push_back(message);
            }
            while let Ok(message) = self.incoming.try_recv() {
                if !ports.contains_key(&message.port) {
                    order.push_back(message.port);
                }
                ports.entry(message.port).or_default().push_back(message);
            }
            // Only the head message of a port is eligible: fragments interleave
            // between ports without reordering messages within any one port.
            let preferred = if control_run < 4 {
                order.iter().position(|port| ports[port][0].control)
            } else {
                order.iter().position(|port| !ports[port][0].control)
            };
            let port = order
                .remove(preferred.unwrap_or(0))
                .expect("nonempty queue");
            let queue = ports.get_mut(&port).expect("queued port");
            let message = queue.front_mut().expect("queued message");
            if message.control {
                control_run += 1;
            } else {
                control_run = 0;
            }
            let end = (message.offset + CHUNK_BYTES).min(message.bytes.len());
            let chunk = Chunk {
                port,
                offset: message.offset as u64,
                last: end == message.bytes.len(),
                bytes: message.bytes.slice(message.offset..end),
            };
            let mut bytes = bytes::BytesMut::new();
            senax_encoder::encode_to(&chunk, &mut bytes)
                .map_err(|_| io::Error::other("encode workset fragment"))?;
            self.socket.write_u32(bytes.len() as u32).await?;
            self.socket.write_all(&bytes).await?;
            message.offset = end;
            if chunk.last {
                queue.pop_front();
            }
            if queue.is_empty() {
                ports.remove(&port);
            } else {
                order.push_back(port);
            }
        }
    }
}

pub(super) struct Receiver {
    socket: tokio::net::unix::OwnedReadHalf,
    partial: HashMap<Port, Vec<u8>>,
}

impl Receiver {
    /// The caller keeps this future alive until completion or connection close;
    /// cancelling a partial frame and resuming would corrupt stream alignment.
    pub async fn next(&mut self) -> io::Result<Packet> {
        loop {
            let length = self.socket.read_u32().await? as usize;
            if length > CHUNK_BYTES + 512 {
                return Err(io::Error::other("oversized workset fragment"));
            }
            let mut encoded = vec![0; length];
            self.socket.read_exact(&mut encoded).await?;
            let mut slice = encoded.as_slice();
            let chunk: Chunk = senax_encoder::decode(&mut slice)
                .map_err(|_| io::Error::other("invalid workset fragment"))?;
            if !slice.is_empty()
                || chunk.bytes.len() > CHUNK_BYTES
                || (!chunk.last && chunk.bytes.is_empty())
            {
                return Err(io::Error::other("invalid workset fragment body"));
            }
            if !self.partial.contains_key(&chunk.port) && self.partial.len() >= MAX_MESSAGES {
                return Err(io::Error::other("too many unfinished workset messages"));
            }
            let bytes = self.partial.entry(chunk.port).or_default();
            if chunk.offset != bytes.len() as u64 {
                return Err(io::Error::other("invalid workset fragment offset"));
            }
            bytes
                .try_reserve(chunk.bytes.len())
                .map_err(io::Error::other)?;
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.last {
                let bytes = self.partial.remove(&chunk.port).expect("inserted above");
                return Ok(Packet {
                    port: chunk.port,
                    bytes: bytes.into(),
                });
            }
        }
    }
}

pub(super) fn connect(
    socket: UnixStream,
) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    let (reader, writer) = socket.into_split();
    let (queue, incoming) = mpsc::unbounded_channel();
    let control = Arc::new(Semaphore::new(8));
    let data = Arc::new(Semaphore::new(MAX_MESSAGES - 8));
    let sender = Sender {
        queue,
        control: control.clone(),
        data: data.clone(),
    };
    let mut writer = Writer {
        socket: writer,
        incoming,
        control_slots: control,
        data_slots: data,
    };
    let writer = tokio::spawn(async move { writer.run().await });
    let receiver = Receiver {
        socket: reader,
        partial: HashMap::new(),
    };
    (sender, receiver, writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn small_other_ports_progress_while_large_message_keeps_its_port_order() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _, writing) = connect(left);
        let (_, mut receiver, other_writing) = connect(right);
        let large = Bytes::from(vec![255; CHUNK_BYTES * 8]);
        sender.send(Port::Terminal(1), large.clone()).await.unwrap();
        sender
            .send(Port::Terminal(1), Bytes::from_static(b"after"))
            .await
            .unwrap();
        sender
            .send(Port::Workset, Bytes::from_static(b"control"))
            .await
            .unwrap();
        let packet = receiver.next().await.unwrap();
        assert_eq!(packet.port, Port::Workset);
        assert_eq!(packet.bytes, b"control"[..]);
        let packet = receiver.next().await.unwrap();
        assert_eq!(packet.port, Port::Terminal(1));
        assert_eq!(packet.bytes, large);
        assert_eq!(receiver.next().await.unwrap().bytes, b"after"[..]);
        drop(sender);
        writing.await.unwrap().unwrap();
        other_writing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn aborting_writer_fails_capacity_waiters() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (sender, _, writing) = connect(left);
        let reserved = sender
            .data
            .clone()
            .acquire_many_owned((MAX_MESSAGES - 8) as u32)
            .await
            .unwrap();
        let sending = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(Port::Workset, Bytes::from(vec![1; CHUNK_BYTES + 1]))
                    .await
            }
        });
        tokio::task::yield_now().await;
        writing.abort();
        let _ = writing.await;
        assert_eq!(
            sending.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(reserved);
    }
}
