//! One workset connection, multiplexed without waiting for a runtime.
//! Large messages yield between fragments. Small control traffic has reserved
//! admission; order is preserved within each logical port, not across ports.
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const CHUNK_BYTES: usize = 64 * 1024;
const MAX_MESSAGES: usize = 32;
const AGENT_WINDOW: usize = 16;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Encode, Decode)]
pub enum Port {
    Workset,
    Agent(rho_agent_types::AgentId),
    Terminal(u64),
    Shell(u64),
}

pub struct Packet {
    pub port: Port,
    pub bytes: Bytes,
    // Travels with agent inbox entries; moving just `bytes` would release early.
    _received: Option<Received>,
}

impl Packet {
    #[doc(hidden)]
    pub fn for_test(port: Port, bytes: Bytes) -> Self {
        Self {
            port,
            bytes,
            _received: None,
        }
    }
}
#[derive(Encode, Decode)]
struct Chunk {
    port: Port,
    offset: u64,
    last: bool,
    consumed: bool,
    // A raw blob: Vec<u8> tags each byte and can exceed the wire chunk limit.
    bytes: Bytes,
}

// Agent credit is returned after decoding, not after socket delivery. Keeping
// this guard in the route inbox bounds it without stalling the shared reader.
struct Received {
    port: Port,
    queue: mpsc::WeakUnboundedSender<Queued>,
}
impl Drop for Received {
    fn drop(&mut self) {
        if let Some(queue) = self.queue.upgrade() {
            let _ = queue.send(Queued::Consumed(self.port));
        }
    }
}
enum Queued {
    Data(Outgoing),
    Consumed(Port),
}
#[derive(Default)]
struct Windows {
    closed: bool,
    agents: HashMap<Port, Arc<Semaphore>>,
}
impl Windows {
    fn close(&mut self) {
        self.closed = true;
        for window in self.agents.values() {
            window.close();
        }
    }
}

struct Outgoing {
    port: Port,
    bytes: Bytes,
    offset: usize,
    control: bool,
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct Sender {
    queue: mpsc::UnboundedSender<Queued>,
    windows: Arc<Mutex<Windows>>,
    control: Arc<Semaphore>,
    data: Arc<Semaphore>,
}

impl Sender {
    pub async fn send(&self, port: Port, bytes: Bytes) -> io::Result<()> {
        let credit = if matches!(port, Port::Agent(_)) {
            let window = {
                let mut windows = self.windows.lock().expect("poison");
                if windows.closed {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                windows
                    .agents
                    .entry(port)
                    .or_insert_with(|| Arc::new(Semaphore::new(AGENT_WINDOW)))
                    .clone()
            };
            Some(
                window
                    .acquire_owned()
                    .await
                    .map_err(|_| io::ErrorKind::BrokenPipe)?,
            )
        } else {
            None
        };
        let control = bytes.len() <= CHUNK_BYTES;
        let slots = if control { &self.control } else { &self.data };
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.queue
            .send(Queued::Data(Outgoing {
                port,
                bytes,
                offset: 0,
                control,
                _permit: permit,
            }))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        if let Some(credit) = credit {
            credit.forget();
        }
        Ok(())
    }

    pub fn close(&self) {
        self.windows.lock().expect("poison").close();
        self.control.close();
        self.data.close();
    }
}

struct Writer {
    socket: tokio::net::unix::OwnedWriteHalf,
    incoming: mpsc::UnboundedReceiver<Queued>,
    windows: Arc<Mutex<Windows>>,
    control_slots: Arc<Semaphore>,
    data_slots: Arc<Semaphore>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.windows.lock().expect("poison").close();
        self.control_slots.close();
        self.data_slots.close();
    }
}

impl Writer {
    async fn write_chunk(&mut self, chunk: Chunk) -> io::Result<()> {
        let mut bytes = bytes::BytesMut::new();
        senax_encoder::encode_to(&chunk, &mut bytes)
            .map_err(|_| io::Error::other("encode workset fragment"))?;
        self.socket.write_u32(bytes.len() as u32).await?;
        self.socket.write_all(&bytes).await
    }

    async fn run(&mut self) -> io::Result<()> {
        let mut ports: HashMap<Port, VecDeque<Outgoing>> = HashMap::new();
        let mut order = VecDeque::new();
        let mut control_run = 0;
        loop {
            if order.is_empty() {
                let Some(message) = self.incoming.recv().await else {
                    return Ok(());
                };
                match message {
                    Queued::Consumed(port) => {
                        self.write_chunk(Chunk {
                            port,
                            offset: 0,
                            last: true,
                            consumed: true,
                            bytes: Bytes::new(),
                        })
                        .await?;
                        continue;
                    }
                    Queued::Data(message) => {
                        order.push_back(message.port);
                        ports.entry(message.port).or_default().push_back(message);
                    }
                }
            }
            while let Ok(message) = self.incoming.try_recv() {
                let message = match message {
                    Queued::Consumed(port) => {
                        self.write_chunk(Chunk {
                            port,
                            offset: 0,
                            last: true,
                            consumed: true,
                            bytes: Bytes::new(),
                        })
                        .await?;
                        continue;
                    }
                    Queued::Data(message) => message,
                };
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
                consumed: false,
                bytes: message.bytes.slice(message.offset..end),
            };
            let last = chunk.last;
            self.write_chunk(chunk).await?;
            message.offset = end;
            if last {
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

pub struct Receiver {
    socket: tokio::net::unix::OwnedReadHalf,
    partial: HashMap<Port, Vec<u8>>,
    windows: Arc<Mutex<Windows>>,
    queue: mpsc::WeakUnboundedSender<Queued>,
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
            if chunk.consumed {
                if !chunk.last || chunk.offset != 0 || !chunk.bytes.is_empty() {
                    return Err(io::Error::other("invalid agent credit"));
                }
                let windows = self.windows.lock().expect("poison");
                let window = windows
                    .agents
                    .get(&chunk.port)
                    .ok_or_else(|| io::Error::other("unknown agent credit"))?;
                if window.available_permits() >= AGENT_WINDOW {
                    return Err(io::Error::other("excess agent credit"));
                }
                window.add_permits(1);
                continue;
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
                    _received: matches!(chunk.port, Port::Agent(_)).then(|| Received {
                        port: chunk.port,
                        queue: self.queue.clone(),
                    }),
                });
            }
        }
    }
}

pub fn connect(socket: UnixStream) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    let (reader, writer) = socket.into_split();
    let (queue, incoming) = mpsc::unbounded_channel();
    let control = Arc::new(Semaphore::new(8));
    let data = Arc::new(Semaphore::new(MAX_MESSAGES - 8));
    let windows = Arc::new(Mutex::new(Windows::default()));
    let weak_queue = queue.downgrade();
    let sender = Sender {
        queue,
        windows: windows.clone(),
        control: control.clone(),
        data: data.clone(),
    };
    let mut writer = Writer {
        socket: writer,
        incoming,
        windows: windows.clone(),
        control_slots: control,
        data_slots: data,
    };
    let writer = tokio::spawn(async move { writer.run().await });
    let receiver = Receiver {
        socket: reader,
        partial: HashMap::new(),
        windows,
        queue: weak_queue,
    };
    (sender, receiver, writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_agent_waits_for_consumption_without_stalling_other_ports() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, mut replies, writing) = connect(left);
        let (_peer, mut receiver, peer_writing) = connect(right);
        let acknowledgments = tokio::spawn(async move { while replies.next().await.is_ok() {} });
        let port = Port::Agent(
            rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(7)).unwrap(),
        );
        let mut held = Vec::new();
        for index in 0..AGENT_WINDOW {
            sender
                .send(port, Bytes::from(vec![index as u8]))
                .await
                .unwrap();
            held.push(receiver.next().await.unwrap());
        }
        let waiting = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send(port, Bytes::from_static(b"last")).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "writing to the socket must not release credit"
        );
        sender
            .send(Port::Workset, Bytes::from_static(b"cancel-other"))
            .await
            .unwrap();
        let control = tokio::time::timeout(std::time::Duration::from_secs(2), receiver.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.port, Port::Workset);
        assert_eq!(control.bytes, b"cancel-other"[..]);
        assert!(!waiting.is_finished());
        drop(held.remove(0));
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let last = receiver.next().await.unwrap();
        assert_eq!(last.port, port);
        assert_eq!(last.bytes, b"last"[..]);
        assert_eq!(
            held.iter()
                .map(|packet| packet.bytes[0])
                .collect::<Vec<_>>(),
            (1..AGENT_WINDOW as u8).collect::<Vec<_>>()
        );
        // A disconnect must also release a producer blocked on receipt credit.
        let blocked = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send(port, Bytes::from_static(b"blocked")).await }
        });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        writing.abort();
        let _ = writing.await;
        assert_eq!(
            blocked.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        acknowledgments.abort();
        peer_writing.abort();
    }

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
