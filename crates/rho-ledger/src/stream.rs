//! Connections exchange log suffixes and forward bytes held from other hosts.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use rho_agent_hosts::{Dialer, HostStream};
use rho_rpc::protocol::{read_frame, write_frame, write_open};
use tokio::sync::broadcast;

use crate::ledger::{Channel, Item, Ledger, Pairing, Received, Verified};
use crate::protocol::{ClientFrame, DeviceId, LogId, Open, ServerFrame};
use crate::secret::Secret;

#[derive(Debug, PartialEq, Eq)]
pub enum LedgerEvent {
    Appended(Vec<Item>),
    Unreadable {
        device: Option<DeviceId>,
    },
    /// Another device's writes arrived, and this device has no phrase to
    /// read them with.
    NeedsKey,
    /// The first host's ledger answered, and the phrase typed before it
    /// did opens what it holds: it is kept now.
    Paired,
    /// The first host's ledger answered, and the phrase typed before it
    /// did opens none of it. The device is back to not having answered.
    PhraseRefused(Secret),
}
pub struct LedgerStreams {
    ledger: Ledger,
    events: futures_mpsc::UnboundedSender<LedgerEvent>,
    changed: broadcast::Sender<()>,
    /// Whether a host's ledger has answered since this device started.
    answered: AtomicBool,
}
impl LedgerStreams {
    pub fn new(ledger: Ledger) -> (Arc<Self>, futures_mpsc::UnboundedReceiver<LedgerEvent>) {
        let (events, receiver) = futures_mpsc::unbounded();
        let (changed, _) = broadcast::channel(256);
        (
            Arc::new(Self {
                ledger,
                events,
                changed,
                answered: AtomicBool::new(false),
            }),
            receiver,
        )
    }
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    pub async fn append(&self, channel: Channel, payloads: Vec<Vec<u8>>) {
        if payloads.is_empty() {
            return;
        }
        let device = self.ledger.device();
        let log = crate::ledger::log_id(device, channel);
        let own = payloads
            .iter()
            .cloned()
            .map(|bytes| Item {
                log,
                device,
                channel,
                bytes,
            })
            .collect();
        self.ledger.append(channel, payloads).await;
        let _ = self.events.unbounded_send(LedgerEvent::Appended(own));
        let _ = self.changed.send(());
    }
    /// Takes the phrase the user typed, with what it opens. Typed before
    /// any host's ledger answered, with nothing held to check it against,
    /// it is held unverified: a typo must not start a second ledger.
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<Pairing> {
        if !self.answered.load(Ordering::SeqCst) && self.ledger.lengths().is_empty() {
            self.ledger.hold_unverified(secret).await?;
            return Ok(self.ledger.pairing());
        }
        let received = self.ledger.set_secret(secret).await?;
        self.report(received, None);
        let _ = self.changed.send(());
        Ok(Pairing::Paired)
    }
    /// A phrase held unverified meets what a host's ledger holds.
    async fn verify(&self, host: &BTreeMap<LogId, u64>) -> anyhow::Result<()> {
        match self.ledger.verify_unverified(host).await? {
            Verified::Nothing | Verified::Waiting => {}
            Verified::Kept(received) => {
                self.report(received, None);
                let _ = self.events.unbounded_send(LedgerEvent::Paired);
                let _ = self.changed.send(());
            }
            Verified::Refused(secret) => {
                let _ = self
                    .events
                    .unbounded_send(LedgerEvent::PhraseRefused(secret));
            }
        }
        Ok(())
    }
    /// Makes a phrase for a new user, and sends what this device said
    /// sealed with it.
    pub async fn make_secret(&self) -> anyhow::Result<Secret> {
        let (secret, received) = self.ledger.make_secret().await?;
        self.report(received, None);
        let _ = self.changed.send(());
        Ok(secret)
    }
    fn report(&self, result: Received, log: Option<LogId>) {
        // A phrase held unverified reads them once it is kept.
        if result.needs_key && self.ledger.pairing() != Pairing::Unverified {
            let _ = self.events.unbounded_send(LedgerEvent::NeedsKey);
        }
        if result.unreadable {
            // A first unreadable record has no authenticated device identity.
            let device = result.device.or_else(|| {
                log.and_then(|log| {
                    self.ledger
                        .items(Channel::Facts)
                        .into_iter()
                        .chain(self.ledger.items(Channel::Notes))
                        .find(|item| item.log == log)
                        .map(|item| item.device)
                })
            });
            let _ = self
                .events
                .unbounded_send(LedgerEvent::Unreadable { device });
        }
        if !result.items.is_empty() {
            let _ = self
                .events
                .unbounded_send(LedgerEvent::Appended(result.items));
        }
    }
    pub fn stream(self: &Arc<Self>) -> Arc<dyn HostStream> {
        Arc::new(LedgerStream(Arc::clone(self)))
    }
    pub async fn speak(
        &self,
        mut reader: impl tokio::io::AsyncRead + Unpin,
        mut writer: impl tokio::io::AsyncWrite + Unpin,
    ) -> anyhow::Result<()> {
        let mut changed = self.changed.subscribe();
        write_frame(
            &mut writer,
            &ClientFrame::Hello {
                have: self.ledger.lengths(),
            },
        )
        .await?;
        let ServerFrame::Lengths { logs } = read_frame(&mut reader).await? else {
            anyhow::bail!("host did not answer hello with lengths")
        };
        let mut host_lengths = logs;
        self.answered.store(true, Ordering::SeqCst);
        self.verify(&host_lengths).await?;
        self.send_missing(&mut writer, &mut host_lengths).await?;
        // A frame read half-way must not be dropped when a change wakes the
        // loop: the stream keeps the read in flight across it.
        let mut frames = std::pin::pin!(futures::stream::unfold(reader, |mut reader| async {
            let frame = read_frame::<_, ServerFrame>(&mut reader).await;
            Some((frame, reader))
        }));
        loop {
            tokio::select! {
                frame = frames.next() => {
                    // Whatever else has arrived is taken in with it.
                    let mut batch = vec![frame.expect("frames never end")?];
                    while batch.len() < 256 {
                        match frames.next().now_or_never() {
                            Some(frame) => batch.push(frame.expect("frames never end")?),
                            None => break,
                        }
                    }
                    for frame in batch {
                        match frame {
                            ServerFrame::Lengths { .. } => anyhow::bail!("host said lengths twice"),
                            ServerFrame::Bytes { log, at, bytes } => {
                                self.receive_bytes(log, at, bytes, &mut host_lengths).await?;
                            }
                        }
                    }
                    self.verify(&host_lengths).await?;
                    self.send_missing(&mut writer, &mut host_lengths).await?;
                }
                change = changed.recv() => {
                    if change.is_err_and(|error| matches!(error, broadcast::error::RecvError::Closed)) { return Ok(()); }
                    self.send_missing(&mut writer, &mut host_lengths).await?;
                }
            }
        }
    }
    async fn send_missing(
        &self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        host: &mut BTreeMap<LogId, u64>,
    ) -> anyhow::Result<()> {
        for (log, len) in self.ledger.lengths() {
            let at = host.get(&log).copied().unwrap_or(0);
            if len > at {
                let chunk = self.ledger.bytes_after(log, at, 64 * 1024);
                let end = at + chunk.len() as u64;
                write_frame(
                    writer,
                    &ClientFrame::Append {
                        log,
                        at,
                        bytes: chunk,
                    },
                )
                .await?;
                host.insert(log, end);
                break;
            }
        }
        Ok(())
    }
}
impl LedgerStreams {
    async fn receive_bytes(
        &self,
        log: LogId,
        at: u64,
        bytes: Vec<u8>,
        host: &mut BTreeMap<LogId, u64>,
    ) -> anyhow::Result<()> {
        // Catch-up and a subscribed live append may overlap. Only matching
        // bytes may overlap.
        let held = self.ledger.lengths().get(&log).copied().unwrap_or(0);
        if at > held {
            anyhow::bail!("ledger gap at {at} after {held}");
        }
        let overlap = usize::try_from(held - at)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        if overlap > 0 {
            let local = self.ledger.bytes_after(log, at, overlap);
            if local[..] != bytes[..overlap] {
                anyhow::bail!("divergent ledger bytes");
            }
        }
        let end = at + bytes.len() as u64;
        host.entry(log)
            .and_modify(|length| *length = (*length).max(end))
            .or_insert(end);
        if overlap < bytes.len() {
            let result = self
                .ledger
                .receive(log, held, bytes[overlap..].to_vec())
                .await;
            self.report(result, Some(log));
            let _ = self.changed.send(());
        }
        Ok(())
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
