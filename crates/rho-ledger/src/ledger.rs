//! Device-owned logs. The host sees only log ids, offsets, and opaque bytes.
use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

use crate::protocol::{DeviceId, LogId};
use crate::seal;
use crate::secret::Secret;

const SELF: TableDefinition<(), Sen<SelfRecord>> = TableDefinition::new("ledger_self_v1");
const LOGS: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("ledger_logs_v2");
const READ: TableDefinition<[u8; 16], u64> = TableDefinition::new("ledger_read_v2");
const UNSENT: TableDefinition<u64, Sen<Envelope>> = TableDefinition::new("ledger_unsent_v2");
// What older builds kept. Every device has converted it; drop it once.
const RETIRED: [&str; 5] = [
    "ledger_merged_v1",
    "ledger_own_v1",
    "ledger_seen_v1",
    "ledger_pending_v1",
    "ledger_segments_v1",
];
const KEY_CONTEXT: &str = "rho 2026-09-24 ledger";

// Keep the old row's exact serialized shape so existing identities and secrets
// survive.
#[derive(Clone, Debug, Encode, Decode)]
struct SelfRecord {
    device: DeviceId,
    secret: Option<[u8; 16]>,
    clock: (u64, u32),
    seq: u64,
    since_base: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Channel {
    Facts,
    Notes,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub log: LogId,
    pub device: DeviceId,
    pub channel: Channel,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, Encode, Decode)]
struct Envelope {
    device: DeviceId,
    channel: Channel,
    payloads: Vec<Vec<u8>>,
}
#[derive(Default)]
pub struct Received {
    pub items: Vec<Item>,
    pub unreadable: bool,
    pub needs_key: bool,
    pub device: Option<DeviceId>,
}

fn append_sealed(write: &mut rho_db::WriteTxn, secret: Secret, envelope: &Envelope) {
    let log = log_id(envelope.device, envelope.channel);
    let mut table = write.open_table(LOGS);
    let mut bytes = table
        .get(log.0)
        .map_or_else(Vec::new, |value| value.value().to_vec());
    let plain = senax_encoder::encode(envelope).expect("encode ledger payloads");
    bytes.extend(seal::seal(
        &secret.derive(KEY_CONTEXT),
        log,
        bytes.len() as u64,
        &plain,
    ));
    table.insert(log.0, bytes.as_slice());
    drop(table);
    write.open_table(READ).insert(log.0, bytes.len() as u64);
}

#[derive(Clone, Debug)]
pub struct Ledger {
    db: RhoDb,
}

pub fn log_id(device: DeviceId, channel: Channel) -> LogId {
    let mut hash = blake3::Hasher::new();
    hash.update(b"rho-ledger/log-id/1");
    hash.update(&device.0);
    hash.update(&[match channel {
        Channel::Facts => 0,
        Channel::Notes => 1,
    }]);
    LogId(hash.finalize().as_bytes()[..16].try_into().unwrap())
}

impl Ledger {
    pub async fn open(db: RhoDb) -> Self {
        let mut write = db.write().await;
        for table in RETIRED {
            write.delete_table(table);
        }
        write.open_table(LOGS);
        write.open_table(READ);
        write.open_table(UNSENT);
        let mut table = write.open_table(SELF);
        if table.get(()).is_none() {
            let mut id = [0; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut id);
            table.insert(
                (),
                SenValue::borrowed(&SelfRecord {
                    device: DeviceId(id),
                    secret: None,
                    clock: (0, 0),
                    seq: 0,
                    since_base: 0,
                }),
            );
        }
        drop(table);
        write.commit();
        Self { db }
    }
    fn me(&self) -> SelfRecord {
        self.db
            .read()
            .open_table(SELF)
            .get(())
            .expect("ledger identity")
            .value()
            .into_owned()
    }
    pub fn device(&self) -> DeviceId {
        self.me().device
    }
    pub fn secret(&self) -> Option<Secret> {
        self.me().secret.map(Secret)
    }
    pub fn items(&self, channel: Channel) -> Vec<Item> {
        let read = self.db.read();
        let Some(secret) = self.secret() else {
            return read
                .open_table(UNSENT)
                .iter()
                .flat_map(|(_, entry)| {
                    let entry = entry.value().into_owned();
                    if entry.channel != channel {
                        return Vec::new();
                    }
                    let log = log_id(entry.device, channel);
                    entry
                        .payloads
                        .into_iter()
                        .map(|bytes| Item {
                            log,
                            device: entry.device,
                            channel,
                            bytes,
                        })
                        .collect()
                })
                .collect();
        };
        let key = secret.derive(KEY_CONTEXT);
        let mut items = Vec::new();
        for (log, bytes) in read.open_table(LOGS).iter() {
            let log = LogId(log.value());
            let bytes = bytes.value();
            let mut at = 0;
            while let Some((size, plain)) = seal::open(&key, log, at as u64, &bytes[at..]) {
                let Ok(envelope) = senax_encoder::decode::<Envelope>(&mut plain.as_slice()) else {
                    break;
                };
                if log_id(envelope.device, envelope.channel) != log {
                    break;
                }
                if envelope.channel == channel {
                    items.extend(envelope.payloads.into_iter().map(|bytes| Item {
                        log,
                        device: envelope.device,
                        channel,
                        bytes,
                    }));
                }
                at += size;
            }
        }
        items
    }
    pub async fn append(&self, channel: Channel, payloads: Vec<Vec<u8>>) {
        if payloads.is_empty() {
            return;
        }
        let mut write = self.db.write().await;
        let me = write
            .open_table(SELF)
            .get(())
            .expect("ledger identity")
            .value()
            .into_owned();
        let envelope = Envelope {
            device: me.device,
            channel,
            payloads,
        };
        if let Some(secret) = me.secret {
            append_sealed(&mut write, Secret(secret), &envelope);
        } else {
            let next = write
                .open_table(UNSENT)
                .iter()
                .next_back()
                .map_or(0, |(key, _)| key.value() + 1);
            write
                .open_table(UNSENT)
                .insert(next, SenValue::borrowed(&envelope));
        }
        write.commit();
    }
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<Received> {
        let mut write = self.db.write().await;
        let mut me = write
            .open_table(SELF)
            .get(())
            .expect("ledger identity")
            .value()
            .into_owned();
        if let Some(held) = me.secret {
            anyhow::ensure!(
                held == secret.0,
                "this device already holds another secret phrase"
            );
            return Ok(Received::default());
        }
        me.secret = Some(secret.0);
        write.open_table(SELF).insert((), SenValue::borrowed(&me));
        let pending: Vec<_> = write
            .open_table(UNSENT)
            .iter()
            .map(|(key, envelope)| (key.value(), envelope.value().into_owned()))
            .collect();
        for (index, envelope) in pending {
            append_sealed(&mut write, secret, &envelope);
            write.open_table(UNSENT).remove(index);
        }
        write.commit();
        let mut received = Received::default();
        for log in self.lengths().keys() {
            let result = self.read_new(*log).await;
            received.items.extend(result.items);
            received.unreadable |= result.unreadable;
        }
        Ok(received)
    }
    pub fn lengths(&self) -> BTreeMap<LogId, u64> {
        self.db
            .read()
            .open_table(LOGS)
            .iter()
            .map(|(log, bytes)| (LogId(log.value()), bytes.value().len() as u64))
            .collect()
    }
    pub fn bytes_after(&self, log: LogId, at: u64) -> Option<Vec<u8>> {
        let read = self.db.read();
        let bytes = read.open_table(LOGS).get(log.0)?.value().to_vec();
        let at = usize::try_from(at).ok()?;
        bytes.get(at..).map(Vec::from)
    }
    /// Appends only at the held length. A divergent or retried append changes
    /// nothing.
    pub async fn receive(&self, log: LogId, at: u64, bytes: Vec<u8>) -> Received {
        if bytes.is_empty() {
            return Received::default();
        }
        let mut write = self.db.write().await;
        let mut table = write.open_table(LOGS);
        let mut held = table
            .get(log.0)
            .map_or_else(Vec::new, |value| value.value().to_vec());
        if held.len() as u64 != at {
            return Received::default();
        }
        held.extend(bytes);
        table.insert(log.0, held.as_slice());
        drop(table);
        write.commit();
        self.read_new(log).await
    }
    async fn read_new(&self, log: LogId) -> Received {
        let mut received = Received::default();
        let Some(secret) = self.secret() else {
            received.needs_key = true;
            return received;
        };
        let key = secret.derive(KEY_CONTEXT);
        let mut write = self.db.write().await;
        let bytes = write
            .open_table(LOGS)
            .get(log.0)
            .expect("held log")
            .value()
            .to_vec();
        let mut at = write
            .open_table(READ)
            .get(log.0)
            .map_or(0, |offset| offset.value()) as usize;
        while at < bytes.len() {
            let Some(length_bytes) = bytes[at..].get(..4) else {
                break;
            };
            let len = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
            if len
                .checked_add(4)
                .is_none_or(|size| size > bytes.len() - at)
            {
                break;
            }
            let Some((size, plain)) = seal::open(&key, log, at as u64, &bytes[at..]) else {
                received.unreadable = true;
                break;
            };
            let Ok(envelope) = senax_encoder::decode::<Envelope>(&mut plain.as_slice()) else {
                received.unreadable = true;
                break;
            };
            if log_id(envelope.device, envelope.channel) != log {
                received.unreadable = true;
                break;
            }
            received.device = Some(envelope.device);
            received
                .items
                .extend(envelope.payloads.into_iter().map(|bytes| Item {
                    log,
                    device: envelope.device,
                    channel: envelope.channel,
                    bytes,
                }));
            at += size;
        }
        write.open_table(READ).insert(log.0, at as u64);
        write.commit();
        received
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_writes_before_key_are_readable_and_sealed_on_key_arrival() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(RhoDb::open(dir.path().join("ledger.redb"))).await;
        ledger
            .append(Channel::Notes, vec![b"unsent".to_vec()])
            .await;
        assert!(ledger.lengths().is_empty());
        assert_eq!(ledger.items(Channel::Notes)[0].bytes, b"unsent");
        ledger.set_secret(Secret::generate()).await.unwrap();
        assert_eq!(ledger.items(Channel::Notes)[0].bytes, b"unsent");
        assert_eq!(ledger.lengths().len(), 1);
    }

    #[tokio::test]
    async fn malformed_record_stops_its_log_without_losing_held_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(RhoDb::open(dir.path().join("ledger.redb"))).await;
        let secret = Secret::generate();
        ledger.set_secret(secret).await.unwrap();
        let other = DeviceId([7; 16]);
        let log = log_id(other, Channel::Facts);
        let key = secret.derive(KEY_CONTEXT);
        let plain = senax_encoder::encode(&Envelope {
            device: other,
            channel: Channel::Facts,
            payloads: vec![b"first".to_vec()],
        })
        .unwrap();
        let valid = seal::seal(&key, log, 0, &plain);
        let mut bad = seal::seal(&key, log, valid.len() as u64, &plain);
        *bad.last_mut().unwrap() ^= 1;
        let trailing = seal::seal(&key, log, (valid.len() + bad.len()) as u64, &plain);
        let mut bytes = valid.clone();
        bytes.extend(bad);
        bytes.extend(trailing);
        let received = ledger.receive(log, 0, bytes.clone()).await;
        assert!(received.unreadable);
        assert_eq!(received.items.len(), 1);
        assert_eq!(ledger.items(Channel::Facts).len(), 1);
        assert_eq!(ledger.lengths()[&log], bytes.len() as u64);
        assert!(ledger.receive(log, 0, bytes).await.items.is_empty());
    }
}
