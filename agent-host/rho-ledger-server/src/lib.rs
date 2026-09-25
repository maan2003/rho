//! Blind append-only byte storage for device logs.
use std::collections::BTreeMap;

use rho_db::RhoDb;
use rho_ledger::protocol::{ClientFrame, LogId, ServerFrame};
use rho_rpc::protocol::{read_frame, write_frame};
use tokio::sync::broadcast;

// What the desk and the older ledger kept here. Every device has carried
// it over; drop it once.
const RETIRED: [&str; 7] = [
    "ledger_segments_v1",
    "rho_desk_facts_v1",
    "rho_desk_fact_verdicts_v1",
    "rho_desk_fact_mutations_v1",
    "rho_desk_note_body_v1",
    "rho_desk_cell_meta_v2",
    "rho_desk_parent_labels_v1",
];
const CHUNK: usize = 64 * 1024;
pub struct LedgerServer {
    db: RhoDb,
    appends: broadcast::Sender<(LogId, u64, Vec<u8>)>,
}
impl LedgerServer {
    pub async fn open(db: RhoDb) -> Self {
        let mut write = db.write().await;
        for table in RETIRED {
            write.delete_table(table);
        }
        rho_ledger::store::open(&mut write);
        write.commit();
        Self {
            db,
            appends: broadcast::channel(1024).0,
        }
    }
    fn lengths(&self) -> BTreeMap<LogId, u64> {
        rho_ledger::store::lengths(&self.db.read())
    }
    async fn append(&self, log: LogId, at: u64, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let mut write = self.db.write().await;
        if !rho_ledger::store::append(&mut write, log, at, &bytes) {
            return;
        }
        write.commit();
        let _ = self.appends.send((log, at, bytes));
    }
    pub async fn serve<R, W>(&self, mut reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let ClientFrame::Hello { have } = read_frame(&mut reader).await? else {
            anyhow::bail!("ledger stream must start with hello")
        };
        let mut appends = self.appends.subscribe();
        write_frame(
            &mut writer,
            &ServerFrame::Lengths {
                logs: self.lengths(),
            },
        )
        .await?;
        // What the device lacks, read a chunk at a time as the stream has
        // room for it.
        let mut catchup: Vec<(LogId, u64, u64)> = self
            .lengths()
            .into_iter()
            .filter_map(|(log, length)| {
                let at = have.get(&log).copied().unwrap_or(0);
                (at < length).then_some((log, at, length))
            })
            .collect();
        loop {
            tokio::select! {
                frame = read_frame::<_, ClientFrame>(&mut reader) => match frame? {
                    ClientFrame::Append { log, at, bytes } => self.append(log, at, bytes).await,
                    ClientFrame::Hello { .. } => anyhow::bail!("ledger stream says hello once"),
                },
                _ = std::future::ready(()), if !catchup.is_empty() => {
                    let (log, at, length) = catchup[0];
                    let bytes = rho_ledger::store::read(&self.db.read(), log, at, CHUNK);
                    let end = at + bytes.len() as u64;
                    if bytes.is_empty() || end >= length {
                        catchup.remove(0);
                    } else {
                        catchup[0].1 = end;
                    }
                    if !bytes.is_empty() {
                        write_frame(&mut writer, &ServerFrame::Bytes { log, at, bytes }).await?;
                    }
                }
                append = appends.recv() => match append {
                    Ok((log, at, bytes)) => write_frame(&mut writer, &ServerFrame::Bytes { log, at, bytes }).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => anyhow::bail!("ledger stream fell behind"),
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
