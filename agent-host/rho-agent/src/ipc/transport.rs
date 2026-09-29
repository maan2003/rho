//! One framed workset connection with bounded I/O queues.
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const MAX_MESSAGES: usize = 32;
// Agent append messages can exceed the public RPC frame limit.
const MAX_FRAME_LEN: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Encode, Decode)]
pub(crate) enum Port {
    Workset,
    Agent(rho_agent_types::AgentId),
    Terminal(u64),
    Shell(u64),
}

#[derive(Debug, Encode, Decode)]
pub(crate) struct Packet {
    pub(crate) port: Port,
    pub(crate) bytes: Bytes,
}

#[cfg(test)]
impl Packet {
    pub(crate) fn for_test(port: Port, bytes: Bytes) -> Self {
        Self { port, bytes }
    }
}

enum Outgoing {
    Packet {
        packet: Packet,
        _permit: OwnedSemaphorePermit,
    },
    /// Everything queued before this has been written; hold the rest until
    /// [`Gate::Resume`].
    Hold(tokio::sync::oneshot::Sender<()>),
}

/// Ahead of the queue: stop or restart writing it.
enum Gate {
    /// Write `packet` now, then nothing from the queue until [`Gate::Resume`].
    Pause(Packet),
    Resume,
}

#[derive(Clone)]
pub(crate) struct Sender {
    queue: mpsc::UnboundedSender<Outgoing>,
    gate: mpsc::UnboundedSender<Gate>,
    slots: Arc<Semaphore>,
}

impl Sender {
    pub async fn send(&self, port: Port, bytes: Bytes) -> io::Result<()> {
        if bytes.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "oversized workset message",
            ));
        }
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.queue
            .send(Outgoing::Packet {
                packet: Packet { port, bytes },
                _permit: permit,
            })
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }

    /// Writes everything sent so far and then holds the queue until
    /// [`Sender::resume`]; returns once held, between frames.
    pub async fn hold(&self) -> io::Result<()> {
        let (held, written) = tokio::sync::oneshot::channel();
        self.queue
            .send(Outgoing::Hold(held))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        written.await.map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    /// Write `bytes` ahead of anything queued, then hold the queue until
    /// [`Sender::resume`]. Sends meanwhile wait in the queue, in order.
    pub fn pause(&self, port: Port, bytes: Bytes) -> io::Result<()> {
        self.gate
            .send(Gate::Pause(Packet { port, bytes }))
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    pub fn resume(&self) -> io::Result<()> {
        self.gate
            .send(Gate::Resume)
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    pub fn close(&self) {
        self.slots.close();
    }
}

struct Writer {
    socket: tokio::net::unix::OwnedWriteHalf,
    incoming: mpsc::UnboundedReceiver<Outgoing>,
    gate: mpsc::UnboundedReceiver<Gate>,
    paused: bool,
    slots: Arc<Semaphore>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.slots.close();
    }
}

impl Writer {
    async fn run(&mut self) -> io::Result<()> {
        loop {
            let outgoing = tokio::select! {
                biased;
                gate = self.gate.recv() => match gate {
                    Some(Gate::Pause(packet)) => {
                        self.paused = true;
                        self.write(&packet).await?;
                        continue;
                    }
                    Some(Gate::Resume) => {
                        self.paused = false;
                        continue;
                    }
                    None => return Ok(()),
                },
                outgoing = self.incoming.recv(), if !self.paused => match outgoing {
                    Some(outgoing) => outgoing,
                    None => return Ok(()),
                },
            };
            match outgoing {
                Outgoing::Packet { packet, .. } => self.write(&packet).await?,
                Outgoing::Hold(held) => {
                    self.paused = true;
                    let _ = held.send(());
                }
            }
        }
    }

    async fn write(&mut self, packet: &Packet) -> io::Result<()> {
        let mut bytes = bytes::BytesMut::new();
        senax_encoder::encode_to(packet, &mut bytes)
            .map_err(|_| io::Error::other("encode workset message"))?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "oversized workset frame",
            ));
        }
        self.socket.write_u32(bytes.len() as u32).await?;
        self.socket.write_all(&bytes).await
    }
}

pub(crate) struct Receiver {
    incoming: mpsc::Receiver<io::Result<Packet>>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::AbortHandle,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

impl Receiver {
    /// Safe to cancel: a dedicated reader finishes any partially read frame.
    pub async fn next(&mut self) -> io::Result<Packet> {
        self.incoming
            .recv()
            .await
            .unwrap_or_else(|| Err(io::ErrorKind::UnexpectedEof.into()))
    }
}

pub(crate) fn connect(
    socket: UnixStream,
) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    let (mut reader, writer) = socket.into_split();
    let (queue, incoming) = mpsc::unbounded_channel();
    let (gate, gates) = mpsc::unbounded_channel();
    let slots = Arc::new(Semaphore::new(MAX_MESSAGES));
    let sender = Sender {
        queue,
        gate,
        slots: slots.clone(),
    };
    let mut writer = Writer {
        socket: writer,
        incoming,
        gate: gates,
        paused: false,
        slots,
    };
    let writer = tokio::spawn(async move { writer.run().await });
    let (received, incoming) = mpsc::channel(MAX_MESSAGES);
    let reader = tokio::spawn(async move {
        loop {
            let result = async {
                let length = reader.read_u32().await? as usize;
                if length > MAX_FRAME_LEN {
                    return Err(io::Error::other("oversized workset frame"));
                }
                let mut encoded = vec![0; length];
                reader.read_exact(&mut encoded).await?;
                let mut slice = encoded.as_slice();
                let packet = senax_encoder::decode(&mut slice)
                    .map_err(|_| io::Error::other("invalid workset frame"))?;
                if !slice.is_empty() {
                    return Err(io::Error::other("invalid workset frame body"));
                }
                Ok(packet)
            }
            .await;
            let failed = result.is_err();
            if received.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let receiver = Receiver {
        incoming,
        reader,
        writer: writer.abort_handle(),
    };
    (sender, receiver, writer)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn large_complete_message_and_mixed_ports_keep_fifo_order() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _reader, writing) = connect(left);
        let (peer, mut receiver, peer_writing) = connect(right);
        let agent = Port::Agent(
            rho_agent_types::AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(7)).unwrap(),
        );
        let large = Bytes::from(vec![255; 512 * 1024 + 3]);
        let messages = [
            (Port::Terminal(1), large),
            (agent, Bytes::from_static(b"agent")),
            (Port::Workset, Bytes::from_static(b"cancel-other")),
            (Port::Shell(2), Bytes::new()),
            (Port::Terminal(1), Bytes::from_static(b"after")),
        ];
        for (port, bytes) in &messages {
            sender.send(*port, bytes.clone()).await.unwrap();
        }
        for (port, bytes) in messages {
            let packet = tokio::time::timeout(Duration::from_secs(2), receiver.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(packet.port, port);
            assert_eq!(packet.bytes, bytes);
        }
        drop(sender);
        writing.await.unwrap().unwrap();
        drop(peer);
        peer_writing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_pause_writes_its_packet_first_and_holds_the_rest_until_resumed() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _reader, _writing) = connect(left);
        let (_peer, mut receiver, _peer_writing) = connect(right);
        // Hold the writer while the queue fills, so the pause jumps it.
        sender
            .pause(Port::Workset, Bytes::from_static(b"first pause"))
            .unwrap();
        sender
            .send(Port::Shell(1), Bytes::from_static(b"held"))
            .await
            .unwrap();
        sender
            .pause(Port::Workset, Bytes::from_static(b"paused"))
            .unwrap();
        assert_eq!(receiver.next().await.unwrap().bytes, b"first pause"[..]);
        assert_eq!(receiver.next().await.unwrap().bytes, b"paused"[..]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.next())
                .await
                .is_err()
        );
        sender.resume().unwrap();
        assert_eq!(receiver.next().await.unwrap().bytes, b"held"[..]);

        // A hold writes what was sent before it and nothing after.
        sender
            .send(Port::Shell(1), Bytes::from_static(b"before"))
            .await
            .unwrap();
        sender.hold().await.unwrap();
        sender
            .send(Port::Shell(1), Bytes::from_static(b"after"))
            .await
            .unwrap();
        assert_eq!(receiver.next().await.unwrap().bytes, b"before"[..]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.next())
                .await
                .is_err()
        );
        sender.resume().unwrap();
        assert_eq!(receiver.next().await.unwrap().bytes, b"after"[..]);
    }

    #[tokio::test]
    async fn cancelling_receive_during_frame_does_not_lose_alignment() {
        let (mut raw, right) = UnixStream::pair().unwrap();
        let (peer, mut receiver, writing) = connect(right);
        let first = senax_encoder::encode(&Packet::for_test(
            Port::Workset,
            Bytes::from_static(b"first"),
        ))
        .unwrap();
        raw.write_u32(first.len() as u32).await.unwrap();
        raw.write_all(&first[..2]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), receiver.next())
                .await
                .is_err()
        );
        raw.write_all(&first[2..]).await.unwrap();
        let second = senax_encoder::encode(&Packet::for_test(
            Port::Terminal(3),
            Bytes::from_static(b"second"),
        ))
        .unwrap();
        raw.write_u32(second.len() as u32).await.unwrap();
        raw.write_all(&second).await.unwrap();
        assert_eq!(receiver.next().await.unwrap().bytes, b"first"[..]);
        let packet = receiver.next().await.unwrap();
        assert_eq!(packet.port, Port::Terminal(3));
        assert_eq!(packet.bytes, b"second"[..]);
        drop(raw);
        assert_eq!(
            receiver.next().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        drop(peer);
        writing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_capacity_waiter_does_not_consume_a_slot() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (sender, _reader, writing) = connect(left);
        let held = sender
            .slots
            .clone()
            .acquire_many_owned(MAX_MESSAGES as u32)
            .await
            .unwrap();
        let cancelled = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(Port::Workset, Bytes::from_static(b"cancelled"))
                    .await
            }
        });
        tokio::task::yield_now().await;
        cancelled.abort();
        let _ = cancelled.await;
        drop(held);
        tokio::time::timeout(
            Duration::from_secs(2),
            sender.send(Port::Workset, Bytes::from_static(b"after cancellation")),
        )
        .await
        .unwrap()
        .unwrap();
        writing.abort();
        let _ = writing.await;
    }

    #[tokio::test]
    async fn close_fails_capacity_waiters() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (sender, _reader, writing) = connect(left);
        let held = sender
            .slots
            .clone()
            .acquire_many_owned(MAX_MESSAGES as u32)
            .await
            .unwrap();
        let waiting = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(Port::Workset, Bytes::from_static(b"blocked"))
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        sender.close();
        assert_eq!(
            waiting.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(held);
        writing.abort();
        let _ = writing.await;
    }

    #[tokio::test]
    async fn dropping_receiver_aborts_both_io_tasks_and_wakes_senders() {
        let (left, mut right) = UnixStream::pair().unwrap();
        let (sender, receiver, writing) = connect(left);
        // Stop the reader in the middle of a frame, not just while idle.
        right.write_u32(100).await.unwrap();
        let reader_task = receiver.reader.abort_handle();
        let held = sender
            .slots
            .clone()
            .acquire_many_owned(MAX_MESSAGES as u32)
            .await
            .unwrap();
        let waiting = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(Port::Workset, Bytes::from_static(b"blocked"))
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(receiver);
        assert!(writing.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !reader_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            waiting.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(held);
    }
    #[tokio::test]
    async fn failed_writer_wakes_blocked_sender() {
        let (left, _right) = UnixStream::pair().unwrap();
        let (sender, _reader, writing) = connect(left);
        let held = sender
            .slots
            .clone()
            .acquire_many_owned(MAX_MESSAGES as u32)
            .await
            .unwrap();
        let waiting = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(Port::Workset, Bytes::from_static(b"blocked"))
                    .await
            }
        });
        tokio::task::yield_now().await;
        writing.abort();
        let _ = writing.await;
        assert_eq!(
            waiting.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(held);
    }
}
