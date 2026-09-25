//! Logs as hosts and devices keep them: each append is its own row, so an
//! append writes only the bytes it adds.
use std::collections::BTreeMap;

use redb::{AccessGuard, TableDefinition, TableHandle};
use rho_db::{ReadTxn, WriteTxn};

use crate::protocol::LogId;

/// `(log, offset)` → the bytes appended at that offset.
const ROWS: TableDefinition<([u8; 16], u64), &[u8]> = TableDefinition::new("ledger_log_rows_v3");
const LENGTHS: TableDefinition<[u8; 16], u64> = TableDefinition::new("ledger_log_lengths_v3");
// Each log as one value, rewritten whole by every append. Remove once every
// device and host has opened a build with rows.
const WHOLE: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("ledger_logs_v2");

pub fn open(write: &mut WriteTxn) {
    write.open_table(ROWS);
    write.open_table(LENGTHS);
    let whole: Vec<([u8; 16], Vec<u8>)> = write
        .open_table(WHOLE)
        .iter()
        .map(|(log, bytes)| (log.value(), bytes.value().to_vec()))
        .collect();
    for (log, bytes) in whole {
        append(write, LogId(log), 0, &bytes);
    }
    write.delete_table(WHOLE.name());
}

pub fn lengths(read: &ReadTxn) -> BTreeMap<LogId, u64> {
    read.open_table(LENGTHS)
        .iter()
        .map(|(log, length)| (LogId(log.value()), length.value()))
        .collect()
}

/// How long `log` is, inside a write.
pub fn length_in(write: &mut WriteTxn, log: LogId) -> u64 {
    write
        .open_table(LENGTHS)
        .get(log.0)
        .map_or(0, |length| length.value())
}

/// Appends only at the held length; anything else changes nothing and
/// says so.
pub fn append(write: &mut WriteTxn, log: LogId, at: u64, bytes: &[u8]) -> bool {
    let mut lengths = write.open_table(LENGTHS);
    if lengths.get(log.0).map_or(0, |length| length.value()) != at {
        return false;
    }
    if bytes.is_empty() {
        return true;
    }
    lengths.insert(log.0, at + bytes.len() as u64);
    drop(lengths);
    write.open_table(ROWS).insert((log.0, at), bytes);
    true
}

/// Up to `max` bytes of `log` from offset `from`.
pub fn read(read: &ReadTxn, log: LogId, from: u64, max: usize) -> Vec<u8> {
    let rows = read.open_table(ROWS);
    let start = row_start(rows.range((log.0, 0)..=(log.0, from)).next_back(), from);
    gather(rows.range((log.0, start)..=(log.0, u64::MAX)), from, max)
}

/// [`read`], inside a write.
pub fn read_in(write: &mut WriteTxn, log: LogId, from: u64, max: usize) -> Vec<u8> {
    let rows = write.open_table(ROWS);
    let start = row_start(rows.range((log.0, 0)..=(log.0, from)).next_back(), from);
    gather(rows.range((log.0, start)..=(log.0, u64::MAX)), from, max)
}

type Row<'a> = (
    AccessGuard<'a, ([u8; 16], u64)>,
    AccessGuard<'a, &'static [u8]>,
);

fn row_start(row: Option<Row<'_>>, from: u64) -> u64 {
    row.map_or(from, |(key, _)| key.value().1)
}

fn gather<'a>(rows: impl Iterator<Item = Row<'a>>, from: u64, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, bytes) in rows {
        let at = key.value().1;
        let bytes = bytes.value();
        let skip = usize::try_from(from.saturating_sub(at)).unwrap_or(usize::MAX);
        let Some(rest) = bytes.get(skip..) else {
            continue;
        };
        let take = rest.len().min(max - out.len());
        out.extend_from_slice(&rest[..take]);
        if out.len() == max {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn rows_read_back_across_their_seams_and_only_append_at_the_end() {
        let db = RhoDb::in_memory();
        let log = LogId([1; 16]);
        let mut write = db.write().await;
        open(&mut write);
        assert!(append(&mut write, log, 0, b"abc"));
        assert!(append(&mut write, log, 3, b"defg"));
        assert!(
            !append(&mut write, log, 3, b"xx"),
            "a stale append is refused"
        );
        assert!(!append(&mut write, log, 9, b"xx"), "a gap is refused");
        write.commit();
        let txn = db.read();
        assert_eq!(lengths(&txn), BTreeMap::from([(log, 7)]));
        assert_eq!(read(&txn, log, 0, usize::MAX), b"abcdefg");
        assert_eq!(read(&txn, log, 2, 3), b"cde");
        assert_eq!(read(&txn, log, 7, usize::MAX), b"");
        assert_eq!(read(&txn, LogId([2; 16]), 0, usize::MAX), b"");
    }

    #[tokio::test]
    async fn whole_logs_move_into_rows() {
        let db = RhoDb::in_memory();
        let log = LogId([1; 16]);
        let mut write = db.write().await;
        write.open_table(WHOLE).insert(log.0, b"held".as_slice());
        open(&mut write);
        write.commit();
        assert!(!db.read().has_table("ledger_logs_v2"));
        let txn = db.read();
        assert_eq!(lengths(&txn), BTreeMap::from([(log, 4)]));
        assert_eq!(read(&txn, log, 0, usize::MAX), b"held");
    }
}
