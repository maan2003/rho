//! Connections exchange log suffixes and forward bytes held from other hosts.
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use futures::channel::mpsc as futures_mpsc;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use rho_agent_hosts::{Dialer, HostStream};
use rho_rpc::protocol::{read_frame, write_frame, write_open};
use tokio::sync::broadcast;

use crate::ledger::{Channel, Item, Ledger, Received};
use crate::notes::Arrived;
use crate::protocol::{BlobHash, ClientFrame, DeviceId, LogId, Open, ServerFrame, SlotId, StoreId};
use crate::secret::Secret;

#[derive(Debug, PartialEq, Eq)]
pub enum LedgerEvent {
    Appended(Vec<Item>),
    /// A note from another device, now the one this device holds.
    Note(Arrived),
    Unreadable {
        device: Option<DeviceId>,
    },
    NeedsKey,
}
/// Whether their note (first) replaces the one this device holds.
pub type KeepTheirs = Box<dyn Fn(&[u8], &[u8]) -> bool + Send + Sync>;
pub struct LedgerStreams {
    ledger: Ledger,
    keep_theirs: KeepTheirs,
    events: futures_mpsc::UnboundedSender<LedgerEvent>,
    changed: broadcast::Sender<()>,
}
impl LedgerStreams {
    pub fn new(
        ledger: Ledger,
        keep_theirs: KeepTheirs,
    ) -> (Arc<Self>, futures_mpsc::UnboundedReceiver<LedgerEvent>) {
        let (events, receiver) = futures_mpsc::unbounded();
        let (changed, _) = broadcast::channel(256);
        (
            Arc::new(Self {
                ledger,
                keep_theirs,
                events,
                changed,
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
    pub async fn put_note(&self, note: [u8; 16], plain: Vec<u8>) {
        self.ledger.put_note(note, plain).await;
        let _ = self.changed.send(());
    }
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<()> {
        let received = self.ledger.set_secret(secret).await?;
        self.report(received, None);
        let _ = self.changed.send(());
        Ok(())
    }
    fn report(&self, result: Received, log: Option<LogId>) {
        if result.needs_key {
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
                slots: self.ledger.slots_seen(),
            },
        )
        .await?;
        let ServerFrame::Lengths { store, logs } = read_frame(&mut reader).await? else {
            anyhow::bail!("host did not answer hello with lengths")
        };
        let mut host_lengths = logs;
        // Puts said and not yet answered, so they are said once.
        let mut putting = HashMap::new();
        self.send_missing(&mut writer, &mut host_lengths).await?;
        self.send_notes(&mut writer, store, &mut putting).await?;
        // A frame read half-way must not be dropped when a change wakes the
        // loop: the stream keeps the read in flight across it.
        let mut frames = std::pin::pin!(futures::stream::unfold(reader, |mut reader| async {
            let frame = read_frame::<_, ServerFrame>(&mut reader).await;
            Some((frame, reader))
        }));
        loop {
            tokio::select! {
                frame = frames.next() => {
                    // Whatever else has arrived is taken in with it: slots in
                    // one write.
                    let mut batch = vec![frame.expect("frames never end")?];
                    while batch.len() < 256 {
                        match frames.next().now_or_never() {
                            Some(frame) => batch.push(frame.expect("frames never end")?),
                            None => break,
                        }
                    }
                    let mut slots = Vec::new();
                    for frame in batch {
                        match frame {
                            ServerFrame::Lengths { .. } => anyhow::bail!("host said lengths twice"),
                            ServerFrame::Slot { slot, version, blob } => {
                                putting.remove(&slot);
                                slots.push((slot, version, blob));
                            }
                            ServerFrame::Bytes { log, at, bytes } => {
                                self.receive_bytes(log, at, bytes, &mut host_lengths).await?;
                            }
                        }
                    }
                    if !slots.is_empty() {
                        let arrived = self
                            .ledger
                            .receive_slots(store, slots, &*self.keep_theirs)
                            .await;
                        if !arrived.is_empty() {
                            let _ = self.changed.send(());
                        }
                        for arrived in arrived {
                            let _ = self.events.unbounded_send(LedgerEvent::Note(arrived));
                        }
                    }
                    self.send_missing(&mut writer, &mut host_lengths).await?;
                    self.send_notes(&mut writer, store, &mut putting).await?;
                }
                change = changed.recv() => {
                    if change.is_err_and(|error| matches!(error, broadcast::error::RecvError::Closed)) { return Ok(()); }
                    self.send_missing(&mut writer, &mut host_lengths).await?;
                    self.send_notes(&mut writer, store, &mut putting).await?;
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
    async fn send_notes(
        &self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        store: StoreId,
        putting: &mut HashMap<SlotId, BlobHash>,
    ) -> anyhow::Result<()> {
        for (slot, prev, blob) in self.ledger.note_puts(store) {
            let hash = *blake3::hash(&blob).as_bytes();
            if putting.get(&slot) == Some(&hash) {
                continue;
            }
            putting.insert(slot, hash);
            write_frame(writer, &ClientFrame::Put { slot, prev, blob }).await?;
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
