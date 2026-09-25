//! A device's side of the ledger: its own entries, everything it has
//! merged, and the segments it publishes. Kept in the client's database.

use std::collections::BTreeMap;
use std::sync::Arc;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

use crate::entry::{Entry, Stamp};
use crate::protocol::{DeviceId, Segment};
use crate::seal;
use crate::secret::Secret;

/// How many segments a device writes before it publishes a base again, so
/// a host holds a bounded run of each device's segments.
const SEGMENTS_PER_BASE: u32 = 64;

/// This device: its id, the secret if it has one, its clock and how many
/// segments it has written. One row.
const SELF: TableDefinition<(), Sen<SelfRecord>> = TableDefinition::new("ledger_self_v1");
/// What this device wrote, newest per key: what a base is made of.
const OWN: TableDefinition<&[u8], Sen<Stored>> = TableDefinition::new("ledger_own_v1");
/// Every device's entries merged, newest per key: what readers read.
const MERGED: TableDefinition<&[u8], Sen<Stored>> = TableDefinition::new("ledger_merged_v1");
/// The newest segment read of every other device. Only grows, so a host
/// cannot hand back an older segment as news.
const SEEN: TableDefinition<[u8; 16], u64> = TableDefinition::new("ledger_seen_v1");
/// Other devices' segments that came before this device had a key, kept
/// to be read once it has one.
const PENDING: TableDefinition<([u8; 16], u64), Sen<Segment>> =
    TableDefinition::new("ledger_pending_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct SelfRecord {
    device: DeviceId,
    secret: Option<[u8; 16]>,
    clock: (u64, u32),
    seq: u64,
    since_base: u32,
}

impl SelfRecord {
    fn key(&self) -> Option<[u8; 32]> {
        self.secret.map(|secret| Secret(secret).derive(KEY_CONTEXT))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct Stored {
    stamp: Stamp,
    value: Option<Vec<u8>>,
}

/// What the ledger's seal key is derived under from the [`Secret`].
const KEY_CONTEXT: &str = "rho 2026-09-24 ledger";

/// A key whose merged value moved, and what it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// What reading a device's segments came to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Received {
    pub changes: Vec<Change>,
    /// A segment did not open with this device's key: the device holds
    /// another key, or the segment was tampered with. Nothing after it was
    /// read.
    pub unreadable: bool,
    /// Segments are waiting for a key this device does not have yet.
    pub needs_key: bool,
}

/// A device's ledger. Cheap to clone; every clone is the same ledger.
#[derive(Clone)]
pub struct Ledger {
    db: RhoDb,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger").finish_non_exhaustive()
    }
}

fn wall_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Merges `entry` into `merged` if it is newer than what is there, and
/// says whether the value it reads as moved.
fn merge(merged: &mut rho_db::WriteTable<'_, &'static [u8], Sen<Stored>>, entry: &Entry) -> bool {
    let current = merged
        .get(entry.key.as_slice())
        .map(|stored| stored.value().into_owned());
    if current
        .as_ref()
        .is_some_and(|current| current.stamp >= entry.stamp)
    {
        return false;
    }
    let moved = current.map(|current| current.value).as_ref() != Some(&entry.value);
    let stored = Stored {
        stamp: entry.stamp,
        value: entry.value.clone(),
    };
    merged.insert(entry.key.as_slice(), SenValue::borrowed(&stored));
    moved
}

impl Ledger {
    /// The ledger in `db`, made on first open with a new device id and no
    /// key.
    pub async fn open(db: RhoDb) -> Self {
        Self::open_with_clock(db, Arc::new(wall_millis)).await
    }

    pub async fn open_with_clock(db: RhoDb, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        let mut write = db.write().await;
        write.open_table(OWN);
        write.open_table(MERGED);
        write.open_table(SEEN);
        write.open_table(PENDING);
        let mut table = write.open_table(SELF);
        if table.get(()).is_none() {
            let mut device = [0; 16];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut device);
            let me = SelfRecord {
                device: DeviceId(device),
                secret: None,
                clock: (0, 0),
                seq: 0,
                since_base: 0,
            };
            table.insert((), SenValue::borrowed(&me));
        }
        drop(table);
        write.commit();
        Self { db, clock }
    }

    fn me(&self) -> SelfRecord {
        self.db
            .read()
            .open_table(SELF)
            .get(())
            .expect("the ledger's own row")
            .value()
            .into_owned()
    }

    pub fn device(&self) -> DeviceId {
        self.me().device
    }

    pub fn secret(&self) -> Option<Secret> {
        self.me().secret.map(Secret)
    }

    /// The newest segment this device has written.
    pub fn head(&self) -> u64 {
        self.me().seq
    }

    /// The newest segment read of every other device, for a host to answer
    /// from.
    pub fn known(&self) -> BTreeMap<DeviceId, u64> {
        self.db
            .read()
            .open_table(SEEN)
            .iter()
            .map(|(device, seq)| (DeviceId(device.value()), seq.value()))
            .collect()
    }

    /// Takes the secret the user's devices share. Returns the base every
    /// host should now hold of this device, and what reading the segments
    /// that came before the secret made of them. Reading with one secret and
    /// then another would mix two ledgers, so a secret is set once.
    pub async fn set_secret(&self, secret: Secret) -> anyhow::Result<(Option<Segment>, Received)> {
        let mut write = self.db.write().await;
        let mut table = write.open_table(SELF);
        let mut me = table
            .get(())
            .expect("the ledger's own row")
            .value()
            .into_owned();
        if let Some(held) = me.secret {
            anyhow::ensure!(
                held == secret.0,
                "this device already holds another secret phrase"
            );
            return Ok((None, Received::default()));
        }
        me.secret = Some(secret.0);
        let key = secret.derive(KEY_CONTEXT);
        table.insert((), SenValue::borrowed(&me));
        drop(table);
        let mut pending: BTreeMap<DeviceId, Vec<Segment>> = BTreeMap::new();
        {
            let mut table = write.open_table(PENDING);
            let held: Vec<(([u8; 16], u64), Segment)> = table
                .iter()
                .map(|(key, segment)| (key.value(), segment.value().into_owned()))
                .collect();
            for ((device, seq), segment) in held {
                table.remove((device, seq));
                pending.entry(DeviceId(device)).or_default().push(segment);
            }
        }
        let mut received = Received::default();
        for (device, segments) in pending {
            read_segments(&mut write, &key, device, segments, &mut received);
        }
        write.commit();
        Ok((self.base_for(0), received))
    }

    /// The merged value at `key`.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db
            .read()
            .open_table(MERGED)
            .get(key)
            .and_then(|stored| stored.value().into_owned().value)
    }

    /// Every merged key starting with `prefix`, with its value, in key
    /// order. Keys taken away are left out.
    pub fn scan(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let read = self.db.read();
        let table = read.open_table(MERGED);
        table
            .range::<&[u8]>(prefix..)
            .map(|(key, stored)| (key.value().to_vec(), stored.value().into_owned()))
            .take_while(|(key, _)| key.starts_with(prefix))
            .filter_map(|(key, stored)| Some((key, stored.value?)))
            .collect()
    }

    /// When each merged key starting with `prefix` was written, as the wall
    /// clock of the device that wrote it read, in milliseconds.
    pub fn stamps(&self, prefix: &[u8]) -> Vec<(Vec<u8>, u64)> {
        let read = self.db.read();
        let table = read.open_table(MERGED);
        table
            .range::<&[u8]>(prefix..)
            .map(|(key, stored)| (key.value().to_vec(), stored.value().into_owned()))
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, stored)| (key, stored.stamp.millis))
            .collect()
    }

    /// Writes `changes` as this device, and returns what moved and the
    /// segment to publish. Without a key the writes still land here; the
    /// base published once a key is set carries them.
    pub async fn write(
        &self,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    ) -> (Vec<Change>, Option<Segment>) {
        if changes.is_empty() {
            return (Vec::new(), None);
        }
        let mut write = self.db.write().await;
        let mut me = write
            .open_table(SELF)
            .get(())
            .expect("the ledger's own row")
            .value()
            .into_owned();
        let mut entries = Vec::with_capacity(changes.len());
        {
            let mut own = write.open_table(OWN);
            for (key, value) in changes {
                let stamp = Stamp::next(me.clock, (self.clock)(), me.device);
                me.clock = (stamp.millis, stamp.counter);
                let stored = Stored {
                    stamp,
                    value: value.clone(),
                };
                own.insert(key.as_slice(), SenValue::borrowed(&stored));
                entries.push(Entry { key, value, stamp });
            }
        }
        let mut moved = Vec::new();
        {
            let mut merged = write.open_table(MERGED);
            for entry in &entries {
                if merge(&mut merged, entry) {
                    moved.push(Change {
                        key: entry.key.clone(),
                        value: entry.value.clone(),
                    });
                }
            }
        }
        me.seq += 1;
        me.since_base += 1;
        let base = me.since_base >= SEGMENTS_PER_BASE;
        if base {
            me.since_base = 0;
        }
        write.open_table(SELF).insert((), SenValue::borrowed(&me));
        write.commit();
        let segment = me.key().map(|key| {
            let entries = if base { self.own_entries() } else { entries };
            Segment {
                seq: me.seq,
                base,
                sealed: seal::seal(&key, me.device, me.seq, base, &entries),
            }
        });
        (moved, segment)
    }

    fn own_entries(&self) -> Vec<Entry> {
        self.db
            .read()
            .open_table(OWN)
            .iter()
            .map(|(key, stored)| {
                let stored = stored.value().into_owned();
                Entry {
                    key: key.value().to_vec(),
                    value: stored.value,
                    stamp: stored.stamp,
                }
            })
            .collect()
    }

    /// A base for a host that holds this device only through `host_head`:
    /// everything this device has written, at its newest segment number.
    /// `None` when the host is not behind, or there is no key to seal with.
    pub fn base_for(&self, host_head: u64) -> Option<Segment> {
        let me = self.me();
        let key = me.key()?;
        if me.seq == 0 || host_head >= me.seq {
            return None;
        }
        Some(Segment {
            seq: me.seq,
            base: true,
            sealed: seal::seal(&key, me.device, me.seq, true, &self.own_entries()),
        })
    }

    /// Reads another device's segments, in order, and merges what they
    /// hold. A segment already read, or older than one read, is skipped.
    /// Without a key they are kept, to be read once there is one.
    pub async fn receive(&self, device: DeviceId, segments: Vec<Segment>) -> Received {
        let mut received = Received::default();
        let me = self.me();
        if device == me.device || segments.is_empty() {
            return received;
        }
        let mut write = self.db.write().await;
        match me.key() {
            Some(key) => read_segments(&mut write, &key, device, segments, &mut received),
            None => {
                received.needs_key = true;
                let mut pending = write.open_table(PENDING);
                for segment in segments {
                    // A base covers everything before it.
                    if segment.base {
                        let older: Vec<u64> = pending
                            .range((device.0, 0)..(device.0, segment.seq))
                            .map(|(key, _)| key.value().1)
                            .collect();
                        for seq in older {
                            pending.remove((device.0, seq));
                        }
                    }
                    pending.insert((device.0, segment.seq), SenValue::borrowed(&segment));
                }
            }
        }
        write.commit();
        received
    }
}

/// Opens `segments` of `device` with `key` and merges what they hold into
/// `received`, advancing what has been seen of the device and the clock.
fn read_segments(
    write: &mut rho_db::WriteTxn,
    key: &[u8; 32],
    device: DeviceId,
    segments: Vec<Segment>,
    received: &mut Received,
) {
    let mut seen = write
        .open_table(SEEN)
        .get(device.0)
        .map_or(0, |seq| seq.value());
    let mut me = write
        .open_table(SELF)
        .get(())
        .expect("the ledger's own row")
        .value()
        .into_owned();
    let mut clock = me.clock;
    let mut moved: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    {
        let mut merged = write.open_table(MERGED);
        for segment in segments {
            if segment.seq <= seen {
                continue;
            }
            let Some(entries) = seal::open(key, device, segment.seq, segment.base, &segment.sealed)
            else {
                received.unreadable = true;
                break;
            };
            for entry in &entries {
                clock = clock.max((entry.stamp.millis, entry.stamp.counter));
                if merge(&mut merged, entry) {
                    moved.insert(entry.key.clone(), entry.value.clone());
                }
            }
            seen = segment.seq;
        }
    }
    write.open_table(SEEN).insert(device.0, seen);
    if clock > me.clock {
        me.clock = clock;
        write.open_table(SELF).insert((), SenValue::borrowed(&me));
    }
    received
        .changes
        .extend(moved.into_iter().map(|(key, value)| Change { key, value }));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct Device {
        ledger: Ledger,
        now: Arc<AtomicU64>,
        _dir: tempfile::TempDir,
    }

    async fn device(secret: Option<Secret>) -> Device {
        let dir = tempfile::tempdir().unwrap();
        let now = Arc::new(AtomicU64::new(1_000));
        let clock = Arc::clone(&now);
        let ledger = Ledger::open_with_clock(
            RhoDb::open(dir.path().join("client.redb")),
            Arc::new(move || clock.load(Ordering::Relaxed)),
        )
        .await;
        if let Some(secret) = secret {
            ledger.set_secret(secret).await.unwrap();
        }
        Device {
            ledger,
            now,
            _dir: dir,
        }
    }

    fn put(key: &str, value: &str) -> (Vec<u8>, Option<Vec<u8>>) {
        (key.as_bytes().to_vec(), Some(value.as_bytes().to_vec()))
    }

    fn value(device: &Device, key: &str) -> Option<String> {
        device
            .ledger
            .get(key.as_bytes())
            .map(|value| String::from_utf8(value).unwrap())
    }

    #[tokio::test]
    async fn another_device_reads_what_one_wrote() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        let (changes, segment) = laptop.ledger.write(vec![put("a", "1")]).await;
        assert_eq!(changes.len(), 1);
        let received = phone
            .ledger
            .receive(laptop.ledger.device(), vec![segment.unwrap()])
            .await;
        assert_eq!(
            received.changes,
            vec![Change {
                key: b"a".to_vec(),
                value: Some(b"1".to_vec())
            }]
        );
        assert_eq!(value(&phone, "a").as_deref(), Some("1"));
        assert_eq!(phone.ledger.known()[&laptop.ledger.device()], 1);
    }

    #[tokio::test]
    async fn the_newest_write_wins_whichever_device_is_read_first() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        let (_, older) = laptop.ledger.write(vec![put("a", "old")]).await;
        phone.now.store(2_000, Ordering::Relaxed);
        let (_, newer) = phone.ledger.write(vec![put("a", "new")]).await;
        laptop
            .ledger
            .receive(phone.ledger.device(), vec![newer.unwrap()])
            .await;
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![older.unwrap()])
            .await;
        assert_eq!(value(&laptop, "a").as_deref(), Some("new"));
        assert_eq!(value(&phone, "a").as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn a_write_made_after_reading_wins_though_its_clock_is_behind() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        laptop.now.store(9_000, Ordering::Relaxed);
        let (_, ahead) = laptop.ledger.write(vec![put("a", "laptop")]).await;
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![ahead.unwrap()])
            .await;
        // The phone's wall clock is far behind the laptop's.
        let (_, reply) = phone.ledger.write(vec![put("a", "phone")]).await;
        laptop
            .ledger
            .receive(phone.ledger.device(), vec![reply.unwrap()])
            .await;
        assert_eq!(value(&laptop, "a").as_deref(), Some("phone"));
    }

    #[tokio::test]
    async fn an_old_segment_handed_back_changes_nothing() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        let laptop_device = laptop.ledger.device();
        let (_, first) = laptop.ledger.write(vec![put("a", "1")]).await;
        let (_, second) = laptop.ledger.write(vec![put("a", "2")]).await;
        phone
            .ledger
            .receive(laptop_device, vec![first.clone().unwrap(), second.unwrap()])
            .await;
        let replayed = phone
            .ledger
            .receive(laptop_device, vec![first.unwrap()])
            .await;
        assert!(replayed.changes.is_empty());
        assert_eq!(value(&phone, "a").as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn a_device_with_another_key_reads_nothing_and_says_so() {
        let laptop = device(Some(Secret::generate())).await;
        let stranger = device(Some(Secret::generate())).await;
        let (_, segment) = laptop.ledger.write(vec![put("a", "1")]).await;
        let received = stranger
            .ledger
            .receive(laptop.ledger.device(), vec![segment.unwrap()])
            .await;
        assert!(received.unreadable);
        assert_eq!(value(&stranger, "a"), None);
        assert_eq!(stranger.ledger.known()[&laptop.ledger.device()], 0);
    }

    #[tokio::test]
    async fn writes_made_before_the_key_reach_others_in_the_first_base() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(None).await, device(Some(secret)).await);
        let (_, segment) = laptop.ledger.write(vec![put("a", "1")]).await;
        assert!(segment.is_none(), "nothing to seal with yet");
        let base = laptop.ledger.set_secret(secret).await.unwrap().0.unwrap();
        assert!(base.base);
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![base])
            .await;
        assert_eq!(value(&phone, "a").as_deref(), Some("1"));
        assert!(laptop.ledger.set_secret(Secret::generate()).await.is_err());
    }

    #[tokio::test]
    async fn segments_that_came_before_the_key_are_read_once_it_is_set() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(None).await);
        let (_, first) = laptop.ledger.write(vec![put("a", "1")]).await;
        let (_, second) = laptop.ledger.write(vec![put("b", "2")]).await;
        let received = phone
            .ledger
            .receive(
                laptop.ledger.device(),
                vec![first.unwrap(), second.unwrap()],
            )
            .await;
        assert!(received.needs_key);
        assert_eq!(value(&phone, "a"), None);
        let (_, received) = phone.ledger.set_secret(secret).await.unwrap();
        assert_eq!(received.changes.len(), 2);
        assert_eq!(value(&phone, "a").as_deref(), Some("1"));
        assert_eq!(value(&phone, "b").as_deref(), Some("2"));
        assert_eq!(phone.ledger.known()[&laptop.ledger.device()], 2);
    }

    #[tokio::test]
    async fn a_host_behind_gets_a_base_of_everything() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        laptop.ledger.write(vec![put("a", "1")]).await;
        laptop.ledger.write(vec![put("b", "2")]).await;
        assert!(laptop.ledger.base_for(2).is_none());
        let base = laptop.ledger.base_for(0).unwrap();
        assert_eq!((base.seq, base.base), (2, true));
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![base])
            .await;
        assert_eq!(value(&phone, "a").as_deref(), Some("1"));
        assert_eq!(value(&phone, "b").as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn every_so_often_a_write_publishes_a_base() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        laptop.ledger.write(vec![put("first", "1")]).await;
        let mut last = None;
        for index in 1..SEGMENTS_PER_BASE {
            last = laptop
                .ledger
                .write(vec![put("n", &index.to_string())])
                .await
                .1;
        }
        let base = last.unwrap();
        assert!(base.base);
        // The base alone brings a new reader everything.
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![base])
            .await;
        assert_eq!(value(&phone, "first").as_deref(), Some("1"));
        assert_eq!(value(&phone, "n").as_deref(), Some("63"));
    }

    #[tokio::test]
    async fn a_taken_away_key_reads_as_absent_everywhere() {
        let secret = Secret::generate();
        let (laptop, phone) = (device(Some(secret)).await, device(Some(secret)).await);
        let (_, first) = laptop.ledger.write(vec![put("label/x", "1")]).await;
        let (_, gone) = laptop.ledger.write(vec![(b"label/x".to_vec(), None)]).await;
        phone
            .ledger
            .receive(laptop.ledger.device(), vec![first.unwrap(), gone.unwrap()])
            .await;
        assert_eq!(value(&phone, "label/x"), None);
        assert!(phone.ledger.scan(b"label/").is_empty());
    }

    #[tokio::test]
    async fn scan_reads_one_prefix_in_key_order() {
        let laptop = device(None).await;
        laptop
            .ledger
            .write(vec![
                put("b/2", "y"),
                put("a/1", "x"),
                put("b/1", "z"),
                put("c", "w"),
            ])
            .await;
        let keys: Vec<_> = laptop
            .ledger
            .scan(b"b/")
            .into_iter()
            .map(|(key, _)| String::from_utf8(key).unwrap())
            .collect();
        assert_eq!(keys, ["b/1", "b/2"]);
    }
}
