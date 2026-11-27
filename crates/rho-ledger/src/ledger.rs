//! Device-owned logs. The host sees only log ids, offsets, and opaque bytes.
use std::collections::BTreeMap;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

use crate::protocol::{DeviceId, LogId};
use crate::secret::Secret;
use crate::{seal, store};

const SELF: TableDefinition<(), Sen<SelfRecord>> = TableDefinition::new("ledger_self_v1");
const READ: TableDefinition<[u8; 16], u64> = TableDefinition::new("ledger_read_v2");
const UNSENT: TableDefinition<u64, Sen<Envelope>> = TableDefinition::new("ledger_unsent_v2");
// What older builds kept, and the note slots that notes synced through
// before they were facts in the log. Every device has converted it; drop
// it once.
const RETIRED: [&str; 8] = [
    "ledger_merged_v1",
    "ledger_own_v1",
    "ledger_seen_v1",
    "ledger_pending_v1",
    "ledger_segments_v1",
    "ledger_notes_v1",
    "ledger_note_hosts_v1",
    "ledger_note_seen_v1",
];
const KEY_CONTEXT: &str = "rho 2026-09-24 ledger";

#[derive(Clone, Debug, Encode, Decode)]
struct SelfRecord {
    device: DeviceId,
    #[senax(default)]
    phrase: Phrase,
    /// Where builds before `phrase` kept the phrase: [`Ledger::open`]
    /// carries it into `phrase`. Drop it once every device has opened
    /// with this build.
    #[senax(skip_default)]
    secret: Option<[u8; 16]>,
    clock: (u64, u32),
    seq: u64,
    since_base: u32,
}
/// What this device holds of the user's phrase, and whether it asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
enum Phrase {
    /// The user has not answered whether they are new or hold a phrase.
    #[default]
    Unasked,
    /// The user chose to keep this device to itself.
    Declined,
    Held([u8; 16]),
    /// The user typed this before any host's ledger answered: nothing is
    /// sealed with it until one does and its logs open with it.
    Unverified([u8; 16]),
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
/// Whether this device holds the user's secret phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pairing {
    /// It holds the phrase: what it says reaches the user's other devices.
    Paired,
    /// The user has not yet said whether they are new or hold a phrase.
    /// What it says stays on it meanwhile.
    Unasked,
    /// The user chose to keep it to itself: what it says stays on it until
    /// the user pairs it.
    Unpaired,
    /// It holds a phrase that no host has checked yet. What it says stays
    /// on it until the first host's ledger answers.
    Unverified,
}

/// What the first host's answer did with a phrase held unverified.
#[derive(Default)]
pub enum Verified {
    /// There was no phrase waiting for an answer.
    #[default]
    Nothing,
    /// The host lists logs whose first record has not arrived yet.
    Waiting,
    /// The phrase opens what the other devices wrote, or nothing was
    /// written: it is kept, and what this device said is sealed.
    Kept(Received),
    /// The phrase opens none of what the other devices wrote. The device
    /// is back to not having answered.
    Refused(Secret),
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
    let at = store::length_in(write, log);
    let plain = senax_encoder::encode(envelope).expect("encode ledger payloads");
    let record = seal::seal(&secret.derive(KEY_CONTEXT), log, at, &plain);
    assert!(
        store::append(write, log, at, &record),
        "own log appends at its end"
    );
    write
        .open_table(READ)
        .insert(log.0, at + record.len() as u64);
}

/// What this device said before it held the phrase, taken out of the
/// unsent table in the order it was said.
fn take_unsent(write: &mut rho_db::WriteTxn) -> Vec<Envelope> {
    let pending: Vec<_> = write
        .open_table(UNSENT)
        .iter()
        .map(|(key, envelope)| (key.value(), envelope.value().into_owned()))
        .collect();
    let mut unsent = write.open_table(UNSENT);
    for (index, _) in &pending {
        unsent.remove(*index);
    }
    pending.into_iter().map(|(_, envelope)| envelope).collect()
}

/// The first record of `log`, once all of it is held.
fn first_record(write: &mut rho_db::WriteTxn, log: LogId) -> Option<Vec<u8>> {
    let head = store::read_in(write, log, 0, 4);
    let len = u32::from_le_bytes(<[u8; 4]>::try_from(head.as_slice()).ok()?) as usize;
    let size = len.saturating_add(4);
    let record = store::read_in(write, log, 0, size);
    (record.len() == size).then_some(record)
}

fn opens_first_record(write: &mut rho_db::WriteTxn, key: &[u8; 32], log: LogId) -> bool {
    first_record(write, log).is_some_and(|record| seal::open(key, log, 0, &record).is_some())
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
        store::open(&mut write);
        write.open_table(READ);
        write.open_table(UNSENT);
        let mut table = write.open_table(SELF);
        if table.get(()).is_none() {
            let mut id = [0; 16];
            rand::TryRng::try_fill_bytes(&mut rand::rngs::SysRng, &mut id).expect("system entropy");
            table.insert(
                (),
                SenValue::borrowed(&SelfRecord {
                    device: DeviceId(id),
                    phrase: Phrase::Unasked,
                    secret: None,
                    clock: (0, 0),
                    seq: 0,
                    since_base: 0,
                }),
            );
        }
        // A phrase an older build kept moves to where it is kept now.
        let mut me = table.get(()).expect("ledger identity").value().into_owned();
        if let Some(secret) = me.secret.take() {
            me.phrase = Phrase::Held(secret);
            table.insert((), SenValue::borrowed(&me));
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
        match self.me().phrase {
            Phrase::Held(secret) => Some(Secret(secret)),
            Phrase::Unasked | Phrase::Declined | Phrase::Unverified(_) => None,
        }
    }
    pub fn pairing(&self) -> Pairing {
        match self.me().phrase {
            Phrase::Held(_) => Pairing::Paired,
            Phrase::Unasked => Pairing::Unasked,
            Phrase::Declined => Pairing::Unpaired,
            Phrase::Unverified(_) => Pairing::Unverified,
        }
    }
    /// The user chose to keep this device to itself, for now: it is not
    /// asked again.
    pub async fn decline(&self) {
        let mut write = self.db.write().await;
        let mut table = write.open_table(SELF);
        let mut me = table.get(()).expect("ledger identity").value().into_owned();
        if me.phrase == Phrase::Unasked {
            me.phrase = Phrase::Declined;
            table.insert((), SenValue::borrowed(&me));
        }
        drop(table);
        write.commit();
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
        for log in store::lengths(&read).into_keys() {
            let bytes = store::read(&read, log, 0, usize::MAX);
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
        if let Phrase::Held(secret) = me.phrase {
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
    /// Makes a new phrase for a user who has none: nothing another device
    /// wrote may be held, since those devices hold a phrase already.
    pub async fn make_secret(&self) -> anyhow::Result<(Secret, Received)> {
        let secret = Secret::generate();
        // Its log starts even when it has said nothing, so that a device
        // that pairs later has something to check the phrase against.
        let received = self.keep_secret(secret, true, |write, _| {
            anyhow::ensure!(
                store::lengths_in(write).is_empty(),
                "your other devices hold a phrase already: type it"
            );
            Ok(())
        });
        Ok((secret, received.await?))
    }
    /// Takes the phrase the user typed. Every log held is another device's.
    /// Twelve real words can still be the wrong phrase (the checksum misses
    /// one typo in sixteen), and a device that took one could never read
    /// the others, so a phrase must open what they wrote before it is kept.
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<Received> {
        self.keep_secret(secret, false, |write, key| {
            let held = store::lengths_in(write);
            anyhow::ensure!(
                held.is_empty() || held.keys().any(|log| opens_first_record(write, key, *log)),
                "this phrase does not open what your other devices wrote"
            );
            Ok(())
        })
        .await
    }
    /// Holds the phrase the user typed before any host's ledger answered,
    /// sealing nothing with it until [`Ledger::verify_unverified`] keeps it.
    pub async fn hold_unverified(&self, secret: Secret) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        let mut table = write.open_table(SELF);
        let mut me = table.get(()).expect("ledger identity").value().into_owned();
        match me.phrase {
            Phrase::Held(held) => anyhow::ensure!(
                held == secret.0,
                "this device already holds another secret phrase"
            ),
            Phrase::Unasked | Phrase::Declined | Phrase::Unverified(_) => {
                me.phrase = Phrase::Unverified(secret.0);
                table.insert((), SenValue::borrowed(&me));
            }
        }
        drop(table);
        write.commit();
        Ok(())
    }
    /// Checks a phrase held unverified against the logs a host lists, once
    /// it holds their first records: it is kept if it opens one of them or
    /// the host lists none, and refused once every first record is held
    /// and none opens.
    pub async fn verify_unverified(
        &self,
        listed: &BTreeMap<LogId, u64>,
    ) -> anyhow::Result<Verified> {
        let Phrase::Unverified(phrase) = self.me().phrase else {
            return Ok(Verified::Nothing);
        };
        let secret = Secret(phrase);
        let key = secret.derive(KEY_CONTEXT);
        let mut write = self.db.write().await;
        let mut opens = false;
        let mut waiting = false;
        for log in listed
            .iter()
            .filter(|(_, len)| **len > 0)
            .map(|(log, _)| *log)
        {
            match first_record(&mut write, log) {
                Some(record) => opens |= seal::open(&key, log, 0, &record).is_some(),
                None => waiting = true,
            }
        }
        if !opens && waiting {
            return Ok(Verified::Waiting);
        }
        let listed_any = listed.values().any(|len| *len > 0);
        if listed_any && !opens {
            let mut table = write.open_table(SELF);
            let mut me = table.get(()).expect("ledger identity").value().into_owned();
            me.phrase = Phrase::Unasked;
            table.insert((), SenValue::borrowed(&me));
            drop(table);
            write.commit();
            return Ok(Verified::Refused(secret));
        }
        drop(write);
        Ok(Verified::Kept(
            self.keep_secret(secret, false, |_, _| Ok(())).await?,
        ))
    }
    /// Keeps `secret` once `fits` says it may, and seals with it what this
    /// device said before it held one. With `start_log` its log starts even
    /// when it has said nothing.
    async fn keep_secret(
        &self,
        secret: Secret,
        start_log: bool,
        fits: impl FnOnce(&mut rho_db::WriteTxn, &[u8; 32]) -> anyhow::Result<()>,
    ) -> anyhow::Result<Received> {
        let mut write = self.db.write().await;
        let mut me = write
            .open_table(SELF)
            .get(())
            .expect("ledger identity")
            .value()
            .into_owned();
        if let Phrase::Held(held) = me.phrase {
            anyhow::ensure!(
                held == secret.0,
                "this device already holds another secret phrase"
            );
            return Ok(Received::default());
        }
        fits(&mut write, &secret.derive(KEY_CONTEXT))?;
        me.phrase = Phrase::Held(secret.0);
        write.open_table(SELF).insert((), SenValue::borrowed(&me));
        let pending = take_unsent(&mut write);
        if start_log && pending.is_empty() {
            append_sealed(
                &mut write,
                secret,
                &Envelope {
                    device: me.device,
                    channel: Channel::Facts,
                    payloads: Vec::new(),
                },
            );
        }
        for envelope in pending {
            append_sealed(&mut write, secret, &envelope);
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
        store::lengths(&self.db.read())
    }
    /// Up to `max` held bytes of `log` from `at`.
    pub fn bytes_after(&self, log: LogId, at: u64, max: usize) -> Vec<u8> {
        store::read(&self.db.read(), log, at, max)
    }
    /// Appends only at the held length. A divergent or retried append changes
    /// nothing.
    pub async fn receive(&self, log: LogId, at: u64, bytes: Vec<u8>) -> Received {
        if bytes.is_empty() {
            return Received::default();
        }
        let mut write = self.db.write().await;
        if !store::append(&mut write, log, at, &bytes) {
            return Received::default();
        }
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
        let from = write
            .open_table(READ)
            .get(log.0)
            .map_or(0, |offset| offset.value());
        let bytes = store::read_in(&mut write, log, from, usize::MAX);
        let mut at = 0;
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
            let Some((size, plain)) = seal::open(&key, log, from + at as u64, &bytes[at..]) else {
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
        write.open_table(READ).insert(log.0, from + at as u64);
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
    async fn the_answer_to_whether_to_pair_outlives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.redb");
        let ledger = Ledger::open(RhoDb::open(&path)).await;
        assert_eq!(ledger.pairing(), Pairing::Unasked);
        ledger.decline().await;
        assert_eq!(ledger.pairing(), Pairing::Unpaired);
        drop(ledger);
        let ledger = Ledger::open(RhoDb::open(&path)).await;
        assert_eq!(ledger.pairing(), Pairing::Unpaired);
        let secret = ledger.set_secret(Secret::generate()).await.map(|_| ());
        assert!(secret.is_ok());
        ledger.decline().await;
        assert_eq!(
            ledger.pairing(),
            Pairing::Paired,
            "declining unpairs nothing"
        );
    }

    /// The row as builds before [`Phrase`] wrote it.
    mod old {
        use super::*;

        #[derive(Clone, Debug, Encode, Decode)]
        pub struct SelfRecord {
            pub device: DeviceId,
            pub secret: Option<[u8; 16]>,
            pub clock: (u64, u32),
            pub seq: u64,
            pub since_base: u32,
        }
    }

    #[tokio::test]
    async fn a_phrase_an_older_build_kept_is_still_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.redb");
        async fn written_by_old_build(path: &std::path::Path, secret: Option<[u8; 16]>) -> Ledger {
            let db = RhoDb::open(path);
            let table: TableDefinition<(), Sen<old::SelfRecord>> =
                TableDefinition::new("ledger_self_v1");
            let mut write = db.write().await;
            write.open_table(table).insert(
                (),
                SenValue::borrowed(&old::SelfRecord {
                    device: DeviceId([3; 16]),
                    secret,
                    clock: (5, 1),
                    seq: 2,
                    since_base: 0,
                }),
            );
            write.commit();
            Ledger::open(db).await
        }
        let secret = Secret::generate();
        let ledger = written_by_old_build(&path, Some(secret.0)).await;
        assert_eq!(ledger.device(), DeviceId([3; 16]));
        assert!(ledger.secret() == Some(secret));
        assert_eq!(ledger.pairing(), Pairing::Paired);
        drop(ledger);
        let ledger = Ledger::open(RhoDb::open(&path)).await;
        assert!(
            ledger.secret() == Some(secret),
            "and after it is written anew"
        );
        drop(ledger);
        std::fs::remove_file(&path).unwrap();
        // One without a phrase was never asked.
        assert_eq!(
            written_by_old_build(&path, None).await.pairing(),
            Pairing::Unasked
        );
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
