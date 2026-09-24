//! The host's side of the ledger: it keeps each device's sealed segments
//! and passes them between the user's devices. It cannot read them, and it
//! merges nothing; each device does that itself (`rho-ledger`).

use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_ledger::protocol::{ClientFrame, DeviceId, Segment, ServerFrame};
use rho_rpc::protocol::{read_frame, write_frame};
use tokio::sync::broadcast;

/// Every device's segments from its latest base on, by device and number.
const SEGMENTS: TableDefinition<([u8; 16], u64), Sen<Segment>> =
    TableDefinition::new("ledger_segments_v1");

pub struct LedgerServer {
    db: RhoDb,
    puts: broadcast::Sender<(DeviceId, Segment)>,
}

impl LedgerServer {
    pub async fn open(db: RhoDb) -> Self {
        let mut write = db.write().await;
        write.open_table(SEGMENTS);
        write.commit();
        Self {
            db,
            puts: broadcast::channel(1024).0,
        }
    }

    fn heads(&self) -> BTreeMap<DeviceId, u64> {
        let read = self.db.read();
        let mut heads = BTreeMap::new();
        for (key, _) in read.open_table(SEGMENTS).iter() {
            let (device, seq) = key.value();
            heads.insert(DeviceId(device), seq);
        }
        heads
    }

    /// Every segment held after `known`, device by device.
    fn after(&self, known: &BTreeMap<DeviceId, u64>) -> BTreeMap<DeviceId, Vec<Segment>> {
        let read = self.db.read();
        let mut segments: BTreeMap<DeviceId, Vec<Segment>> = BTreeMap::new();
        for (key, segment) in read.open_table(SEGMENTS).iter() {
            let (device, seq) = key.value();
            let device = DeviceId(device);
            if seq > known.get(&device).copied().unwrap_or(0) {
                segments
                    .entry(device)
                    .or_default()
                    .push(segment.value().into_owned());
            }
        }
        segments
    }

    /// Keeps a device's segment if it is newer than what is held, and
    /// passes it on. A base replaces everything before it.
    async fn put(&self, device: DeviceId, segment: Segment) {
        let mut write = self.db.write().await;
        let mut table = write.open_table(SEGMENTS);
        let held: Vec<u64> = table
            .range((device.0, 0)..=(device.0, u64::MAX))
            .map(|(key, _)| key.value().1)
            .collect();
        if held.last().is_some_and(|head| *head >= segment.seq) {
            return;
        }
        if segment.base {
            for seq in held {
                table.remove((device.0, seq));
            }
        }
        table.insert((device.0, segment.seq), SenValue::borrowed(&segment));
        drop(table);
        write.commit();
        let _ = self.puts.send((device, segment));
    }

    /// A ledger stream, for as long as it lasts.
    pub async fn serve<R, W>(&self, mut reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let ClientFrame::Hello { known } = read_frame(&mut reader).await? else {
            anyhow::bail!("a ledger stream must start with hello");
        };
        // Before reading what is held, so nothing put in between is lost;
        // a segment heard twice is skipped by the reader.
        let mut puts = self.puts.subscribe();
        write_frame(
            &mut writer,
            &ServerFrame::Heads {
                heads: self.heads(),
            },
        )
        .await?;
        for (device, segments) in self.after(&known) {
            write_frame(&mut writer, &ServerFrame::Segments { device, segments }).await?;
        }
        let (frames_tx, mut frames) = tokio::sync::mpsc::channel(16);
        let read = async move {
            loop {
                let frame: ClientFrame = read_frame(&mut reader).await?;
                if frames_tx.send(frame).await.is_err() {
                    return anyhow::Ok(());
                }
            }
        };
        let handle = async {
            loop {
                tokio::select! {
                    frame = frames.recv() => match frame {
                        Some(ClientFrame::Put { device, segment }) => {
                            self.put(device, segment).await;
                        }
                        Some(ClientFrame::Hello { .. }) => {
                            anyhow::bail!("a ledger stream says hello once");
                        }
                        None => return Ok(()),
                    },
                    put = puts.recv() => match put {
                        Ok((device, segment)) => {
                            write_frame(&mut writer, &ServerFrame::Segments {
                                device,
                                segments: vec![segment],
                            })
                            .await?;
                        }
                        // The device reconnects and hears what it missed.
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            anyhow::bail!("the ledger stream fell behind");
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    },
                }
            }
        };
        tokio::select! {
            result = read => result,
            result = handle => result,
        }
    }
}

#[cfg(test)]
mod tests;
