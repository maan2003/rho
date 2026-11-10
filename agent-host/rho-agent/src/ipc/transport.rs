//! One framed workset connection with bounded I/O queues.
//!
//! A packet can take a file descriptor with it ([`Sender::send_with_fd`]),
//! which is how a socket of the workset's gets to the host: the two
//! processes may not see the same files, and a descriptor needs no path.
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::Arc;

use bytes::Bytes;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncWriteExt as _, Interest};
use tokio::net::UnixStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

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

#[derive(Clone, Debug, Encode, Decode)]
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
        /// Sent with the packet's first byte.
        fd: Option<OwnedFd>,
        _permit: OwnedSemaphorePermit,
    },
    /// Everything queued before this has been written; hold the rest until
    /// [`Gate::Resume`].
    Hold(tokio::sync::oneshot::Sender<()>),
}

/// Ahead of the queue, for a held writer.
enum Gate {
    Resume,
    /// A copy of everything still queued, which stays queued.
    Queued(tokio::sync::oneshot::Sender<Vec<Packet>>),
}

#[derive(Clone)]
pub(crate) struct Sender {
    queue: mpsc::UnboundedSender<Outgoing>,
    gate: mpsc::UnboundedSender<Gate>,
    slots: Arc<Semaphore>,
}

impl Sender {
    pub async fn send(&self, port: Port, bytes: Bytes) -> io::Result<()> {
        self.queue(port, bytes, None).await
    }

    /// [`Sender::send`], and `fd` with the packet: the receiver gets a
    /// descriptor of the same open file beside it. This side's is closed
    /// once the packet is written, or dropped.
    ///
    /// Only for a packet no hand-over looks at ([`Sender::queued`] copies
    /// packets without their descriptors): one from a worker.
    pub async fn send_with_fd(&self, port: Port, bytes: Bytes, fd: OwnedFd) -> io::Result<()> {
        self.queue(port, bytes, Some(fd)).await
    }

    async fn queue(&self, port: Port, bytes: Bytes, fd: Option<OwnedFd>) -> io::Result<()> {
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
                fd,
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

    /// What a held writer has yet to write.
    pub async fn queued(&self) -> io::Result<Vec<Packet>> {
        let (taken, packets) = tokio::sync::oneshot::channel();
        self.gate
            .send(Gate::Queued(taken))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        packets.await.map_err(|_| io::ErrorKind::BrokenPipe.into())
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
    /// Queued packets a hold looked at, written before the rest.
    backlog: std::collections::VecDeque<Outgoing>,
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
                    Some(Gate::Queued(taken)) => {
                        while let Ok(outgoing) = self.incoming.try_recv() {
                            self.backlog.push_back(outgoing);
                        }
                        let _ = taken.send(
                            self.backlog
                                .iter()
                                .filter_map(|outgoing| match outgoing {
                                    Outgoing::Packet { packet, .. } => Some(packet.clone()),
                                    Outgoing::Hold(_) => None,
                                })
                                .collect(),
                        );
                        continue;
                    }
                    Some(Gate::Resume) => {
                        self.paused = false;
                        continue;
                    }
                    None => return Ok(()),
                },
                outgoing = async { self.backlog.pop_front() }, if !self.paused && !self.backlog.is_empty() => outgoing.expect("backlog"),
                outgoing = self.incoming.recv(), if !self.paused => match outgoing {
                    Some(outgoing) => outgoing,
                    None => return Ok(()),
                },
            };
            match outgoing {
                Outgoing::Packet { packet, fd, .. } => self.write(&packet, fd).await?,
                Outgoing::Hold(held) => {
                    self.paused = true;
                    let _ = held.send(());
                }
            }
        }
    }

    async fn write(&mut self, packet: &Packet, fd: Option<OwnedFd>) -> io::Result<()> {
        let mut bytes = bytes::BytesMut::new();
        senax_encoder::encode_to(packet, &mut bytes)
            .map_err(|_| io::Error::other("encode workset message"))?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "oversized workset frame",
            ));
        }
        let length = (bytes.len() as u32).to_be_bytes();
        let Some(fd) = fd else {
            self.socket.write_all(&length).await?;
            return self.socket.write_all(&bytes).await;
        };
        // The descriptor goes with the frame's first bytes, so the reader
        // has it by the time it has the frame, and with no other frame.
        let socket: &UnixStream = self.socket.as_ref();
        let sent = loop {
            socket.writable().await?;
            let sent = socket.try_io(Interest::WRITABLE, || {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                let fds = [fd.as_fd()];
                control.push(SendAncillaryMessage::ScmRights(&fds));
                Ok(rustix::net::sendmsg(
                    socket,
                    &[io::IoSlice::new(&length)],
                    &mut control,
                    SendFlags::NOSIGNAL,
                )?)
            });
            match sent {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                sent => break sent?,
            }
        };
        self.socket.write_all(&length[sent..]).await?;
        self.socket.write_all(&bytes).await
    }
}

/// Fills `buffer` from `socket`, and takes what descriptors came with those
/// bytes. It asks for no byte beyond the buffer, so with a buffer that ends
/// where a frame does it takes no descriptor of a later frame.
///
/// Cancelling it loses nothing only while `buffer` is one byte: one byte is
/// read whole or not at all.
async fn read_exact(
    socket: &UnixStream,
    mut buffer: &mut [u8],
    fds: &mut Vec<OwnedFd>,
) -> io::Result<()> {
    while !buffer.is_empty() {
        socket.readable().await?;
        let read = socket.try_io(Interest::READABLE, || {
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let received = rustix::net::recvmsg(
                socket,
                &mut [io::IoSliceMut::new(buffer)],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            )?;
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(received) = message {
                    fds.extend(received);
                }
            }
            Ok(received.bytes)
        });
        match read {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// A packet as it was read, and the descriptor that came with it, if one
/// did.
pub(crate) type Received = (Packet, Option<OwnedFd>);

pub(crate) struct Receiver {
    incoming: mpsc::Receiver<io::Result<Received>>,
    /// Whether the reader starts another frame.
    reading: watch::Sender<bool>,
    /// Whether the reader is stopped between frames.
    parked: watch::Receiver<bool>,
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
    pub async fn next(&mut self) -> io::Result<Received> {
        self.incoming
            .recv()
            .await
            .unwrap_or_else(|| Err(io::ErrorKind::UnexpectedEof.into()))
    }

    /// Stops reading between frames and returns what was read but not yet
    /// received; the rest stays in the socket until [`Receiver::resume`].
    pub async fn stop(&mut self) -> io::Result<Vec<Received>> {
        self.reading.send_replace(false);
        let mut read = Vec::new();
        let mut parked = self.parked.clone();
        // Receive while waiting: the reader may be blocked on a full channel.
        loop {
            tokio::select! {
                biased;
                result = parked.wait_for(|parked| *parked) => {
                    result.map_err(|_| io::Error::from(io::ErrorKind::UnexpectedEof))?;
                    break;
                }
                packet = self.incoming.recv() => {
                    read.push(packet.unwrap_or_else(|| Err(io::ErrorKind::UnexpectedEof.into()))?);
                }
            }
        }
        while let Ok(packet) = self.incoming.try_recv() {
            read.push(packet?);
        }
        Ok(read)
    }

    pub fn resume(&self) {
        self.reading.send_replace(true);
    }
}

pub(crate) fn connect(
    socket: UnixStream,
) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    open(socket, true)
}

/// [`connect`], with the reader stopped until [`Receiver::resume`].
pub(crate) fn connect_stopped(
    socket: UnixStream,
) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    open(socket, false)
}

fn open(
    socket: UnixStream,
    reading: bool,
) -> (Sender, Receiver, tokio::task::JoinHandle<io::Result<()>>) {
    let (reader, writer) = socket.into_split();
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
        backlog: Default::default(),
        slots,
    };
    let writer = tokio::spawn(async move { writer.run().await });
    let (received, incoming) = mpsc::channel(MAX_MESSAGES);
    let (reading, mut read) = watch::channel(reading);
    let (parking, parked) = watch::channel(false);
    let reader = tokio::spawn(async move {
        let socket: &UnixStream = reader.as_ref();
        // The descriptors that came with the frame being read.
        let mut fds = Vec::new();
        loop {
            // Between frames: a stop takes effect here, never mid-frame.
            let first = tokio::select! {
                biased;
                _ = async { read.wait_for(|read| !*read).await.is_ok() } => {
                    parking.send_replace(true);
                    if read.wait_for(|read| *read).await.map(|_| ()).is_err() {
                        break;
                    }
                    parking.send_replace(false);
                    continue;
                }
                // One byte is all or nothing, so losing this race reads nothing.
                first = async {
                    let mut first = [0];
                    read_exact(socket, &mut first, &mut fds).await.map(|()| first[0])
                } => first,
            };
            let result = async {
                let mut length = [first?, 0, 0, 0];
                read_exact(socket, &mut length[1..], &mut fds).await?;
                let length = u32::from_be_bytes(length) as usize;
                if length > MAX_FRAME_LEN {
                    return Err(io::Error::other("oversized workset frame"));
                }
                let mut encoded = vec![0; length];
                read_exact(socket, &mut encoded, &mut fds).await?;
                let mut slice = encoded.as_slice();
                let packet = senax_encoder::decode(&mut slice)
                    .map_err(|_| io::Error::other("invalid workset frame"))?;
                if !slice.is_empty() {
                    return Err(io::Error::other("invalid workset frame body"));
                }
                // A packet takes one descriptor with it; more are closed.
                Ok((packet, fds.drain(..).next()))
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
        reading,
        parked,
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
            let (packet, fd) = tokio::time::timeout(Duration::from_secs(2), receiver.next())
                .await
                .unwrap()
                .unwrap();
            assert!(fd.is_none());
            assert_eq!(packet.port, port);
            assert_eq!(packet.bytes, bytes);
        }
        drop(sender);
        writing.await.unwrap().unwrap();
        drop(peer);
        peer_writing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_hold_writes_what_came_before_and_hands_over_the_rest() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _reader, _writing) = connect(left);
        let (_peer, mut receiver, _peer_writing) = connect(right);
        sender
            .send(Port::Shell(1), Bytes::from_static(b"before"))
            .await
            .unwrap();
        sender.hold().await.unwrap();
        sender
            .send(Port::Shell(1), Bytes::from_static(b"after"))
            .await
            .unwrap();
        assert_eq!(receiver.next().await.unwrap().0.bytes, b"before"[..]);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.next())
                .await
                .is_err()
        );
        let queued = sender.queued().await.unwrap();
        assert_eq!(
            queued
                .iter()
                .map(|packet| &packet.bytes[..])
                .collect::<Vec<_>>(),
            [&b"after"[..]]
        );
        // Looking does not reorder: the looked-at packet still goes first.
        sender
            .send(Port::Shell(1), Bytes::from_static(b"later"))
            .await
            .unwrap();
        sender.resume().unwrap();
        assert_eq!(receiver.next().await.unwrap().0.bytes, b"after"[..]);
        assert_eq!(receiver.next().await.unwrap().0.bytes, b"later"[..]);
    }

    #[tokio::test]
    async fn a_stopped_reader_hands_over_what_it_read_and_leaves_the_rest() {
        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _reader, _writing) = connect(left);
        let (_peer, mut receiver, _peer_writing) = connect(right);
        // More than the reader's channel holds, so it is blocked mid-stream.
        let frame = |index: usize| Bytes::from(format!("{index}:{}", "x".repeat(index * 997)));
        for index in 0..40 {
            sender.send(Port::Shell(1), frame(index)).await.unwrap();
        }
        let mut seen = vec![receiver.next().await.unwrap().0.bytes];
        tokio::time::sleep(Duration::from_millis(50)).await;
        seen.extend(
            receiver
                .stop()
                .await
                .unwrap()
                .into_iter()
                .map(|(packet, _)| packet.bytes),
        );
        for index in 40..45 {
            sender.send(Port::Shell(1), frame(index)).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.next())
                .await
                .is_err(),
            "a stopped reader reads nothing more"
        );
        receiver.resume();
        while seen.len() < 45 {
            seen.push(receiver.next().await.unwrap().0.bytes);
        }
        assert_eq!(seen, (0..45).map(frame).collect::<Vec<_>>());
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
        assert_eq!(receiver.next().await.unwrap().0.bytes, b"first"[..]);
        let (packet, _) = receiver.next().await.unwrap();
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

    /// A descriptor arrives with the packet it was sent with and no other,
    /// whatever the packets around it are: small ones that are all queued
    /// before the reader takes one, and one that takes many reads. It is
    /// the same open file, and the sender's own is closed by then.
    #[tokio::test]
    async fn a_descriptor_arrives_with_its_packet() {
        use std::io::{Read as _, Write as _};
        use std::os::unix::net::UnixStream as Pipe;

        let (left, right) = UnixStream::pair().unwrap();
        let (sender, _reader, _writing) = connect(left);
        let (_peer, mut receiver, _peer_writing) = connect(right);
        // Which packets take a descriptor, and how long each is.
        let plan = [
            (false, 3),
            (true, 5),
            (true, 0),
            (false, 300_000),
            (true, 300_000),
            (false, 1),
            (true, 2),
        ];
        let mut kept = Vec::new();
        for (index, (with_fd, len)) in plan.into_iter().enumerate() {
            let bytes = Bytes::from(vec![index as u8; len]);
            if with_fd {
                let (mut ours, theirs) = Pipe::pair().unwrap();
                ours.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                ours.write_all(&[index as u8]).unwrap();
                kept.push(Some(ours));
                sender
                    .send_with_fd(Port::Workset, bytes, theirs.into())
                    .await
                    .unwrap();
            } else {
                kept.push(None);
                sender.send(Port::Workset, bytes).await.unwrap();
            }
        }
        // Everything is written, or as much as the socket takes, before
        // anything is taken.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for (index, (with_fd, len)) in plan.into_iter().enumerate() {
            let (packet, fd) = tokio::time::timeout(Duration::from_secs(2), receiver.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(packet.bytes, vec![index as u8; len], "packet {index}");
            assert_eq!(fd.is_some(), with_fd, "packet {index}");
            let (Some(fd), Some(mut ours)) = (fd, kept[index].take()) else {
                continue;
            };
            // The other end of the pair this packet took, and no other's.
            let mut theirs = Pipe::from(fd);
            let mut mark = [0];
            theirs.read_exact(&mut mark).unwrap();
            assert_eq!(mark, [index as u8]);
            // Only the receiver holds it now: closing it ends the pair.
            drop(theirs);
            assert_eq!(ours.read(&mut mark).unwrap(), 0, "packet {index}");
        }
    }
}
