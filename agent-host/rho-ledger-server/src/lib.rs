//! Blind append-only byte storage for device logs.
use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::RhoDb;
use rho_ledger::protocol::{ClientFrame, LogId, ServerFrame};
use rho_rpc::protocol::{read_frame, write_frame};
use tokio::sync::broadcast;

const LOGS: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("ledger_logs_v2");
pub struct LedgerServer {
    db: RhoDb,
    appends: broadcast::Sender<(LogId, u64, Vec<u8>)>,
}
impl LedgerServer {
    pub async fn open(db: RhoDb) -> Self {
        let mut write = db.write().await;
        write.open_table(LOGS);
        write.commit();
        Self {
            db,
            appends: broadcast::channel(1024).0,
        }
    }
    fn lengths(&self) -> BTreeMap<LogId, u64> {
        self.db
            .read()
            .open_table(LOGS)
            .iter()
            .map(|(log, bytes)| (LogId(log.value()), bytes.value().len() as u64))
            .collect()
    }
    fn after(&self, have: &BTreeMap<LogId, u64>) -> Vec<(LogId, u64, Vec<u8>)> {
        self.db
            .read()
            .open_table(LOGS)
            .iter()
            .filter_map(|(log, bytes)| {
                let log = LogId(log.value());
                let at = have.get(&log).copied().unwrap_or(0);
                let bytes = bytes.value().get(usize::try_from(at).ok()?..)?.to_vec();
                Some((log, at, bytes))
            })
            .flat_map(|(log, at, bytes)| {
                bytes
                    .chunks(64 * 1024)
                    .enumerate()
                    .map(move |(index, chunk)| {
                        (log, at + (index * 64 * 1024) as u64, chunk.to_vec())
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    async fn append(&self, log: LogId, at: u64, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let mut write = self.db.write().await;
        let mut table = write.open_table(LOGS);
        let mut held = table
            .get(log.0)
            .map_or_else(Vec::new, |value| value.value().to_vec());
        if held.len() as u64 != at {
            return;
        }
        held.extend_from_slice(&bytes);
        table.insert(log.0, held.as_slice());
        drop(table);
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
        let mut catchup = self.after(&have).into_iter();
        loop {
            tokio::select! {
                frame = read_frame::<_, ClientFrame>(&mut reader) => match frame? {
                    ClientFrame::Append { log, at, bytes } => self.append(log, at, bytes).await,
                    ClientFrame::Hello { .. } => anyhow::bail!("ledger stream says hello once"),
                },
                _ = std::future::ready(()), if catchup.len() > 0 => {
                    if let Some((log, at, bytes)) = catchup.next() {
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
