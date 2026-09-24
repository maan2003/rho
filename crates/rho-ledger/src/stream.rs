//! The ledger's stream to each host: this device's segments out to every
//! host, every other device's segments in.
//!
//! On every connection the device says what it has read, hears the host's
//! heads and what it missed, and hands the host a base of its own if the
//! host is behind on it. After that it puts each segment it writes and
//! reads each one another device puts.

use std::sync::Arc;

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use rho_agent_hosts::{Dialer, HostStream};
use rho_rpc::protocol::{read_frame, write_frame, write_open};
use tokio::sync::broadcast;

use crate::ledger::{Change, Ledger};
use crate::protocol::{ClientFrame, DeviceId, Open, Segment, ServerFrame};
use crate::secret::Secret;

/// What the ledger's streams hear.
#[derive(Debug, PartialEq, Eq)]
pub enum LedgerEvent {
    /// Another device's writes moved these keys.
    Changed(Vec<Change>),
    /// A device's segments do not open with this device's key.
    Unreadable { device: DeviceId },
    /// Other devices wrote, and this device has no key to read them with.
    NeedsKey,
}

/// The ledger and its stream to every host. Writes go through here so
/// that every host hears them.
pub struct LedgerStreams {
    ledger: Ledger,
    events: futures_mpsc::UnboundedSender<LedgerEvent>,
    puts: broadcast::Sender<Segment>,
}

impl LedgerStreams {
    /// The receiver hears what every host's stream reads.
    pub fn new(ledger: Ledger) -> (Arc<Self>, futures_mpsc::UnboundedReceiver<LedgerEvent>) {
        let (events, events_rx) = futures_mpsc::unbounded();
        let (puts, _) = broadcast::channel(256);
        let streams = Arc::new(Self {
            ledger,
            events,
            puts,
        });
        (streams, events_rx)
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Writes `changes` and puts them to every host, returning what moved.
    pub async fn write(&self, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Vec<Change> {
        let (moved, segment) = self.ledger.write(changes).await;
        if let Some(segment) = segment {
            let _ = self.puts.send(segment);
        }
        moved
    }

    /// Takes the secret the user's devices share, puts everything this
    /// device wrote before it to every host, and reads what other devices
    /// wrote while it had none.
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<()> {
        let (base, received) = self.ledger.set_secret(secret).await?;
        if let Some(base) = base {
            let _ = self.puts.send(base);
        }
        self.report(received);
        Ok(())
    }

    /// The ledger's stream for a host, for the host to open on every
    /// connection.
    pub fn stream(self: &Arc<Self>) -> Arc<dyn HostStream> {
        Arc::new(LedgerStream(Arc::clone(self)))
    }
}

impl LedgerStreams {
    fn report(&self, received: crate::Received) {
        if received.needs_key {
            let _ = self.events.unbounded_send(LedgerEvent::NeedsKey);
        }
        if !received.changes.is_empty() {
            let _ = self
                .events
                .unbounded_send(LedgerEvent::Changed(received.changes));
        }
    }
}

struct LedgerStream(Arc<LedgerStreams>);

impl HostStream for LedgerStream {
    fn name(&self) -> &'static str {
        "ledger"
    }

    fn run(&self, dialer: Dialer) -> BoxFuture<'static, anyhow::Result<()>> {
        let streams = Arc::clone(&self.0);
        Box::pin(async move {
            let mut socket = dialer.open(Some(50)).await?;
            write_open(&mut socket, &Open).await?;
            let (reader, writer) = tokio::io::split(socket);
            streams.speak(reader, writer).await
        })
    }
}

impl LedgerStreams {
    /// One connection's stream, for as long as it lasts.
    pub async fn speak(
        &self,
        mut reader: impl tokio::io::AsyncRead + Unpin,
        mut writer: impl tokio::io::AsyncWrite + Unpin,
    ) -> anyhow::Result<()> {
        // Before saying hello, so no write falls between the host's heads
        // and the first put.
        let mut puts = self.puts.subscribe();
        let device = self.ledger.device();
        write_frame(
            &mut writer,
            &ClientFrame::Hello {
                known: self.ledger.known(),
            },
        )
        .await?;
        let ServerFrame::Heads { heads } = read_frame(&mut reader).await? else {
            anyhow::bail!("the host did not answer the ledger's hello with its heads");
        };
        let head = heads.get(&device).copied().unwrap_or(0);
        if let Some(base) = self.ledger.base_for(head) {
            write_frame(
                &mut writer,
                &ClientFrame::Put {
                    device,
                    segment: base,
                },
            )
            .await?;
        }
        let read = async {
            loop {
                let ServerFrame::Segments { device, segments } = read_frame(&mut reader).await?
                else {
                    anyhow::bail!("the host said its heads twice");
                };
                let received = self.ledger.receive(device, segments).await;
                if received.unreadable {
                    let _ = self
                        .events
                        .unbounded_send(LedgerEvent::Unreadable { device });
                }
                self.report(received);
            }
        };
        let write = async {
            loop {
                let segment = match puts.recv().await {
                    Ok(segment) => Some(segment),
                    // A base at the newest segment covers whatever was
                    // missed.
                    Err(broadcast::error::RecvError::Lagged(_)) => self.ledger.base_for(0),
                    Err(broadcast::error::RecvError::Closed) => return anyhow::Ok(()),
                };
                if let Some(segment) = segment {
                    write_frame(&mut writer, &ClientFrame::Put { device, segment }).await?;
                }
            }
        };
        tokio::select! {
            result = read => result,
            result = write => result,
        }
    }
}
