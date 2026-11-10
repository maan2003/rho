//! One byte stream over a port of the workset connection.
//!
//! A GCCP byte stream ([docs/gccp.md](../../../../docs/gccp.md)) ends at a
//! Unix socket that the workset process holds, and the host holds the other
//! harness's end. The bytes between the two cross here, on a
//! [`Port::Stream`] the two sides attached, and both run [`pipe`].
//!
//! A port is not a pipe: the connection is shared, and a side that cannot
//! keep up with a port loses the port. So a side sends [`WINDOW`] chunks
//! and then waits for the other to say that it wrote one. That is the
//! stream's backpressure, from one end's socket to the other's, and it
//! bounds what a port has in flight.

use std::sync::atomic::Ordering;

use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use senax_encoder::{Decode, Encode};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{Semaphore, mpsc};

use super::transport::{Port, Sender};
use super::workset::{decode, encode};

/// The most bytes in one chunk.
const CHUNK: usize = 64 * 1024;
/// The chunks a side sends before the other said that it wrote one. A port
/// then holds at most this many chunks, as many acks, and an end: fewer
/// than its queue takes.
pub(crate) const WINDOW: usize = 8;

#[derive(Encode, Decode)]
enum Frame {
    Data(Bytes),
    /// The oldest chunk not yet acked has been written.
    Ack,
    /// No more bytes come this way. The other way stays open.
    End,
}

/// Carries bytes between `port` and one end of a stream: what `reader`
/// gives goes out on the port, and what comes in on the port goes to
/// `writer`. Each way ends on its own, as the two halves of a socket do,
/// and the pipe returns once both have.
///
/// An error is a stream that broke, on this side or the other. Whoever
/// holds the port then drops it, and the other side's pipe ends the same
/// way.
pub(crate) async fn pipe<R, W>(
    port: Port,
    sender: &Sender,
    incoming: &mut mpsc::Receiver<Bytes>,
    mut reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let window = Semaphore::new(WINDOW);
    // The other side holds to the window, so this is never full.
    let (chunks, mut written) = mpsc::channel::<Option<Bytes>>(WINDOW + 1);
    let ended = std::sync::atomic::AtomicBool::new(false);
    let send = async |frame: Frame| -> anyhow::Result<()> {
        Ok(sender.send(port, encode(&frame)?).await?)
    };
    let out = async {
        loop {
            let mut chunk = BytesMut::with_capacity(CHUNK);
            if reader.read_buf(&mut chunk).await? == 0 {
                send(Frame::End).await?;
                ended.store(true, Ordering::Relaxed);
                return Ok(());
            }
            window.acquire().await.expect("never closed").forget();
            send(Frame::Data(chunk.freeze())).await?;
        }
    };
    // Reads the port whatever the writer does: an ack must not wait behind
    // bytes that this end is slow to take. Returns when the port closes
    // after the other side's end.
    let route = async {
        let mut over = false;
        loop {
            let Some(frame) = incoming.recv().await else {
                anyhow::ensure!(over, "the stream broke");
                return Ok(());
            };
            let chunk = match decode(&frame)? {
                Frame::Ack => {
                    window.add_permits(1);
                    continue;
                }
                Frame::Data(chunk) => Some(chunk),
                Frame::End => None,
            };
            anyhow::ensure!(!over, "bytes after the stream's end");
            over = chunk.is_none();
            chunks
                .try_send(chunk)
                .ok()
                .context("more bytes than the stream's window")?;
        }
    };
    let write = async {
        while let Some(chunk) = written.recv().await.context("the stream broke")? {
            writer.write_all(&chunk).await?;
            send(Frame::Ack).await?;
        }
        writer.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };
    let both = async { tokio::try_join!(out, write).map(|_| ()) };
    tokio::pin!(both);
    tokio::select! {
        done = &mut both => return done,
        closed = route => closed?,
    }
    // The other side is done and has dropped the port. It ended only after
    // this side's end, so all that is left is to write what it sent.
    anyhow::ensure!(ended.load(Ordering::Relaxed), "the stream broke");
    both.await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::DuplexStream;
    use tokio::net::UnixStream;

    use super::*;
    use crate::ipc::transport;

    /// Two pipes on one port of a workset connection, as the host and the
    /// worker run them, and the far ends of the two streams they join. Each
    /// side drops the port when its pipe returns, as both processes do.
    fn joined() -> (DuplexStream, DuplexStream) {
        let port = Port::Stream(7);
        let (left, right) = UnixStream::pair().unwrap();
        let (a, a_near) = tokio::io::duplex(1024);
        let (b, b_near) = tokio::io::duplex(1024);
        for (socket, near) in [(left, a_near), (right, b_near)] {
            let (sender, mut receiver, _writer) = transport::connect(socket);
            // As the routes of both processes are: a port that is behind
            // loses its packet, and an empty packet closes the port.
            let (route, mut incoming) = mpsc::channel(32);
            // A connection writes what is queued only while it has a sender.
            let held = sender.clone();
            tokio::spawn(async move {
                let _held = held;
                let mut route = Some(route);
                while let Ok(packet) = receiver.next().await {
                    assert_eq!(packet.port, port);
                    if packet.bytes.is_empty() {
                        route = None;
                    } else if let Some(route) = &route {
                        let full = matches!(
                            route.try_send(packet.bytes),
                            Err(mpsc::error::TrySendError::Full(_))
                        );
                        assert!(!full, "the port overflowed");
                    }
                }
            });
            tokio::spawn(async move {
                let (reader, writer) = tokio::io::split(near);
                let piped = pipe(port, &sender, &mut incoming, reader, writer).await;
                let _ = sender.send(port, Bytes::new()).await;
                piped
            });
        }
        (a, b)
    }

    fn bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    async fn within<T>(work: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("the stream stalled")
    }

    /// Megabytes cross both ways at once, whole and in order, though each
    /// end takes them through a small buffer and the port's queue is short:
    /// the window holds a sender back rather than overflow the port.
    #[tokio::test]
    async fn bytes_cross_both_ways_without_overflowing_the_port() {
        let (a, b) = joined();
        let (there, back) = (bytes(3_000_000, 1), bytes(2_000_001, 2));
        let (mut a_read, mut a_write) = tokio::io::split(a);
        let (mut b_read, mut b_write) = tokio::io::split(b);
        let (mut at_a, mut at_b) = (Vec::new(), Vec::new());
        within(async {
            tokio::join!(
                async {
                    a_write.write_all(&there).await.unwrap();
                    a_write.shutdown().await.unwrap();
                },
                async {
                    b_write.write_all(&back).await.unwrap();
                    b_write.shutdown().await.unwrap();
                },
                async {
                    // An end that is slow to start holds the other back.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    b_read.read_to_end(&mut at_b).await.unwrap()
                },
                async { a_read.read_to_end(&mut at_a).await.unwrap() },
            )
        })
        .await;
        assert!(at_b == there, "a to b");
        assert!(at_a == back, "b to a");
    }

    /// One way ends and the other goes on: a request that ends with its
    /// sender's end of stream still gets its whole answer, though the
    /// answering side's pipe is done, and drops the port, before the asking
    /// side has written the answer out.
    #[tokio::test]
    async fn one_way_ends_while_the_other_goes_on() {
        let (mut a, mut b) = joined();
        // As much as the window takes, so that b is rid of it while a's end
        // has taken almost none.
        let answer = bytes(6000, 5);
        within(async {
            a.write_all(b"question").await.unwrap();
            a.shutdown().await.unwrap();
            let mut asked = Vec::new();
            b.read_to_end(&mut asked).await.unwrap();
            assert_eq!(asked, b"question");
            b.write_all(&answer).await.unwrap();
            b.shutdown().await.unwrap();
            // The answer is in the port by now, and a has read none of it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut answered = Vec::new();
            a.read_to_end(&mut answered).await.unwrap();
            assert!(answered == answer);
        })
        .await;
    }

    /// An end that writes everything before it reads anything is not
    /// stopped by what waits for it: the acks of its bytes do not queue
    /// behind the bytes it has not taken.
    #[tokio::test]
    async fn an_end_that_does_not_read_still_sends() {
        let (mut a, b) = joined();
        let (there, back) = (bytes(4_000_000, 3), bytes(4_000_000, 4));
        let (mut b_read, mut b_write) = tokio::io::split(b);
        let (mut at_a, mut at_b) = (Vec::new(), Vec::new());
        within(async {
            tokio::join!(
                async {
                    a.write_all(&there).await.unwrap();
                    a.shutdown().await.unwrap();
                    a.read_to_end(&mut at_a).await.unwrap();
                },
                async {
                    b_write.write_all(&back).await.unwrap();
                    b_write.shutdown().await.unwrap();
                },
                async { b_read.read_to_end(&mut at_b).await.unwrap() },
            )
        })
        .await;
        assert!(at_b == there, "a to b");
        assert!(at_a == back, "b to a");
    }

    /// A stream whose other side went away mid-way is an error, not an end:
    /// the end of this side sees its socket close without the rest.
    #[tokio::test]
    async fn a_port_that_closes_early_breaks_the_stream() {
        let port = Port::Stream(1);
        let (left, _right) = UnixStream::pair().unwrap();
        let (sender, _receiver, _writer) = transport::connect(left);
        let (route, mut incoming) = mpsc::channel(32);
        let (_far, near) = tokio::io::duplex(64);
        route
            .send(encode(&Frame::Data(Bytes::from_static(b"half"))).unwrap())
            .await
            .unwrap();
        drop(route);
        let (reader, writer) = tokio::io::split(near);
        let piped = within(pipe(port, &sender, &mut incoming, reader, writer)).await;
        assert!(piped.is_err());
    }
}
